//! The checked-`throws` analysis (spec §7.2): every error a function can throw, by `throw` or
//! by calling a throwing function, is caught by an enclosing `try` or declared in the
//! function's `throws` clause. Functions that may leave the clause out (lambdas, top-level
//! functions) get what they throw inferred instead.

use ppl_diag::{Diagnostic, Span, codes};

use crate::hir::{Expr, ExprKind, FnId, PatKind, Pattern, Program, Stmt, StructId, Target};
use crate::{Ty, Types};

/// Reports unhandled errors in every function with a declared (or no) `throws` clause, after
/// settling the inferred clauses of recursive top-level functions.
pub fn check(program: &Program, types: &mut Types) -> Vec<Diagnostic> {
    for _ in 0..32 {
        let mut changed = false;
        for (i, f) in program.fns.iter().enumerate() {
            if f.infer_throws && f.closure.is_none() {
                let t = uncaught(program, types, FnId(i as u32));
                if t != types.fn_throws[i] {
                    types.fn_throws[i] = t;
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }
    let mut diags = Vec::new();
    for (i, f) in program.fns.iter().enumerate() {
        let id = FnId(i as u32);
        if f.infer_throws {
            continue;
        }
        let mut w = Walk::new(program, types, id, Mode::Report(&mut diags));
        w.body();
    }
    diags
}

/// The errors function `id` lets escape: what it should infer as its `throws`.
pub fn uncaught(program: &Program, types: &Types, id: FnId) -> Vec<Ty> {
    let mut out = Vec::new();
    Walk::new(program, types, id, Mode::Collect(&mut out)).body();
    normalize(program, out)
}

/// The functions with inferred `throws` that `id` calls directly (to check them first).
pub fn callees(program: &Program, types: &Types, id: FnId) -> Vec<FnId> {
    let mut out = Vec::new();
    let mut w = Walk::new(program, types, id, Mode::Callees(&mut out));
    w.body();
    out
}

/// Sorted, without duplicates or classes whose base class is also listed.
fn normalize(p: &Program, mut ts: Vec<Ty>) -> Vec<Ty> {
    let key = |t: &Ty| match t {
        Ty::Class(s, ..) => (0, s.0),
        Ty::Param(o, i, _) => (1 + *o, *i),
        _ => (u32::MAX, 0),
    };
    ts.sort_by_key(key);
    ts.dedup();
    let classes: Vec<StructId> = ts
        .iter()
        .filter_map(|t| match t {
            Ty::Class(s, ..) => Some(*s),
            _ => None,
        })
        .collect();
    ts.retain(|t| match t {
        Ty::Class(s, ..) => !classes.iter().any(|c| c != s && p.is_subclass(*s, *c)),
        _ => true,
    });
    ts
}

/// The class a `catch` clause catches every instance of (`None`: everything);
/// `Some(NOTHING)` when the pattern can fail on a value of its type.
fn catches(p: &Program, pat: &Pattern) -> Option<StructId> {
    match &pat.kind {
        PatKind::Wild | PatKind::Bind(_) => None,
        PatKind::As(_, inner) => catches(p, inner),
        PatKind::Type {
            ty: Ty::Class(sid, ..),
            fields,
            ..
        } => {
            let total = fields.as_ref().is_none_or(|fs| {
                fs.iter()
                    .all(|f| matches!(f.pat.kind, PatKind::Wild | PatKind::Bind(_)))
            });
            if !total {
                Some(NOTHING)
            } else if Some(*sid) == p.error_class {
                None
            } else {
                Some(*sid)
            }
        }
        _ => Some(NOTHING),
    }
}

const NOTHING: StructId = StructId(u32::MAX);

enum Mode<'m> {
    Report(&'m mut Vec<Diagnostic>),
    Collect(&'m mut Vec<Ty>),
    Callees(&'m mut Vec<FnId>),
}

struct Walk<'a, 'm> {
    p: &'a Program,
    types: &'a Types,
    fun: FnId,
    /// The top level may throw anything: an uncaught error ends the program.
    all: bool,
    /// The enclosing `try`s' catch clauses (`None`: catches everything), innermost last.
    handlers: Vec<Vec<Option<StructId>>>,
    mode: Mode<'m>,
}

impl<'a, 'm> Walk<'a, 'm> {
    fn new(p: &'a Program, types: &'a Types, fun: FnId, mode: Mode<'m>) -> Self {
        Walk {
            p,
            types,
            fun,
            all: fun == p.entry || p.static_inits.contains(&fun),
            handlers: Vec::new(),
            mode,
        }
    }

    fn body(&mut self) {
        let f = self.p.func(self.fun);
        for st in &f.body.stmts {
            self.stmt(st);
        }
        if let Some(t) = &f.body.tail {
            self.expr(t);
        }
    }

    fn stmt(&mut self, st: &Stmt) {
        match st {
            Stmt::Let { init: Some(e), .. } | Stmt::Expr(e) => self.expr(e),
            Stmt::Let { init: None, .. } => {}
        }
    }

    fn expr(&mut self, e: &Expr) {
        match &e.kind {
            ExprKind::Throw(inner) => {
                self.expr(inner);
                let t = self.types.exprs[inner.id.0 as usize].clone();
                if let Ty::Class(..) = t {
                    self.raise(&t, e.span, None);
                }
            }
            ExprKind::Try {
                body,
                catches: cs,
                finally,
            } => {
                let set = cs.iter().map(|c| catches(self.p, &c.pat)).collect();
                self.handlers.push(set);
                self.expr(body);
                self.handlers.pop();
                for c in cs {
                    self.expr(&c.body);
                }
                if let Some(f) = finally {
                    self.expr(f);
                }
            }
            ExprKind::TryOpt(inner) | ExprKind::TryBang(inner) => {
                self.handlers.push(vec![None]);
                self.expr(inner);
                self.handlers.pop();
            }
            _ => {
                if let Some(callee) = self.callee(e) {
                    if let Mode::Callees(out) = &mut self.mode {
                        if self.p.func(callee).infer_throws && !out.contains(&callee) {
                            out.push(callee);
                        }
                    } else {
                        let mut ts = self.types.fn_throws[callee.0 as usize].clone();
                        if let Some(args) = self.types.instances.get(&e.id) {
                            ts = ts.iter().map(|t| t.subst(callee.0, args)).collect();
                        }
                        for t in &ts {
                            self.raise(t, e.span, Some(callee));
                        }
                    }
                } else if let ExprKind::Call { callee, .. } = &e.kind
                    && let Ty::Fn(_, _, ts) = &self.types.exprs[callee.id.0 as usize]
                {
                    // Calling a function value.
                    for t in ts.iter() {
                        self.raise(t, e.span, None);
                    }
                }
                e.for_each_child(&mut |c| self.expr(c));
            }
        }
    }

    /// The user function a call-like expression calls, if any.
    fn callee(&self, e: &Expr) -> Option<FnId> {
        let t = self.types;
        if let Some(id) = t
            .methods
            .get(&e.id)
            .or_else(|| t.overloads.get(&e.id))
            .or_else(|| t.ctor_calls.get(&e.id))
            .or_else(|| t.op_calls.get(&e.id))
            .or_else(|| t.getters.get(&e.id))
        {
            return Some(*id);
        }
        match &e.kind {
            ExprKind::Call { callee, .. } => match callee.kind {
                ExprKind::Fn(id) => Some(id),
                _ => None,
            },
            ExprKind::SuperCall { method, .. } => Some(*method),
            ExprKind::Assign {
                target: Target::Field { id, .. },
                ..
            } => t.setters.get(id).copied(),
            _ => None,
        }
    }

    fn caught(&self, t: &Ty) -> bool {
        self.handlers.iter().any(|h| {
            h.iter().any(|c| match (c, t) {
                (None, _) => true,
                (Some(c), Ty::Class(s, ..)) => *c != NOTHING && self.p.is_subclass(*s, *c),
                _ => false,
            })
        })
    }

    fn raise(&mut self, t: &Ty, span: Span, via: Option<FnId>) {
        if matches!(t, Ty::Never | Ty::Error) || self.caught(t) {
            return;
        }
        let diags = match &mut self.mode {
            Mode::Callees(_) => return,
            Mode::Collect(out) => {
                out.push(t.clone());
                return;
            }
            Mode::Report(d) => d,
        };
        if self.all {
            return;
        }
        let f = self.p.func(self.fun);
        let declared = f.throws.iter().any(|d| match (d, t) {
            (Ty::Class(d, ..), Ty::Class(s, ..)) => self.p.is_subclass(*s, *d),
            (d, t) => d == t,
        });
        if declared {
            return;
        }
        let name = t.to_string();
        let label = match via {
            Some(g) => format!("`{}` can throw `{name}`", self.p.func(g).name),
            None => match t {
                Ty::Class(..) if via.is_none() => format!("`{name}` is thrown here"),
                _ => format!("can throw `{name}`"),
            },
        };
        diags.push(
            Diagnostic::error(codes::UNHANDLED_ERROR, format!("unhandled error `{name}`"))
                .primary(span, label)
                .help(format!(
                    "catch it with `try … catch (e: {name}) …`, or add `throws {name}` to `{}`",
                    f.name
                )),
        );
    }
}
