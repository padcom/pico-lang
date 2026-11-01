//! The checked-`throws` analysis (spec §7.2): every error a function can throw, by `throw` or
//! by calling a throwing function, is caught by an enclosing `try` or declared in the
//! function's `throws` clause.

use ppl_diag::{Diagnostic, Span, codes};

use crate::hir::{ExprKind, FnId, PatKind, Pattern, Program, Stmt, StructId, Target};
use crate::{Ty, Types};

pub fn check(program: &Program, types: &Types) -> Vec<Diagnostic> {
    let mut diags = Vec::new();
    for (i, f) in program.fns.iter().enumerate() {
        let id = FnId(i as u32);
        let mut w = Walk {
            p: program,
            types,
            fun: id,
            // The top level may throw anything: an uncaught error ends the program.
            all: id == program.entry || program.static_inits.contains(&id),
            handlers: Vec::new(),
            diags: &mut diags,
        };
        for st in &f.body.stmts {
            w.stmt(st);
        }
        if let Some(t) = &f.body.tail {
            w.expr(t);
        }
    }
    diags
}

/// The class a `catch` clause catches every instance of (`None`: everything); `Some(None)`
/// when the pattern can fail on a value of its type, so it catches nothing for certain.
fn catches(p: &Program, pat: &Pattern) -> Option<Option<StructId>> {
    match &pat.kind {
        PatKind::Wild | PatKind::Bind(_) => Some(None),
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
                return Some(Some(StructId(u32::MAX)));
            }
            if Some(*sid) == p.error_class {
                Some(None)
            } else {
                Some(Some(*sid))
            }
        }
        _ => Some(Some(StructId(u32::MAX))),
    }
}

struct Walk<'a> {
    p: &'a Program,
    types: &'a Types,
    fun: FnId,
    all: bool,
    /// The enclosing `try`s' catch clauses, innermost last.
    handlers: Vec<Vec<Option<StructId>>>,
    diags: &'a mut Vec<Diagnostic>,
}

impl Walk<'_> {
    fn stmt(&mut self, st: &Stmt) {
        match st {
            Stmt::Let { init: Some(e), .. } | Stmt::Expr(e) => self.expr(e),
            Stmt::Let { init: None, .. } => {}
        }
    }

    fn expr(&mut self, e: &crate::hir::Expr) {
        match &e.kind {
            ExprKind::Throw(inner) => {
                self.expr(inner);
                if let Ty::Class(sid, ..) = &self.types.exprs[inner.id.0 as usize] {
                    self.raise(*sid, e.span, None);
                }
            }
            ExprKind::Try {
                body,
                catches: cs,
                finally,
            } => {
                let set = cs.iter().filter_map(|c| catches(self.p, &c.pat)).collect();
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
            _ => {
                if let Some(callee) = self.callee(e) {
                    for t in &self.p.func(callee).throws {
                        if let Ty::Class(sid, ..) = t {
                            self.raise(*sid, e.span, Some(callee));
                        }
                    }
                }
                e.for_each_child(&mut |c| self.expr(c));
            }
        }
    }

    /// The user function a call-like expression calls, if any.
    fn callee(&self, e: &crate::hir::Expr) -> Option<FnId> {
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

    fn raise(&mut self, sid: StructId, span: Span, via: Option<FnId>) {
        let caught = self.handlers.iter().any(|h| {
            h.iter()
                .any(|c| c.is_none_or(|c| c.0 != u32::MAX && self.p.is_subclass(sid, c)))
        });
        if caught || self.all {
            return;
        }
        let f = self.p.func(self.fun);
        let declared = f.throws.iter().any(|t| match t {
            Ty::Class(d, ..) => self.p.is_subclass(sid, *d),
            _ => false,
        });
        if declared {
            return;
        }
        let name = &self.p.struct_def(sid).name;
        let label = match via {
            Some(g) => format!("`{}` can throw `{name}`", self.p.func(g).name),
            None => format!("`{name}` is thrown here"),
        };
        let mut d = Diagnostic::error(codes::UNHANDLED_ERROR, format!("unhandled error `{name}`"))
            .primary(span, label)
            .help(format!(
                "catch it with `try … catch (e: {name}) …`, or add `throws {name}` to `{}`",
                f.name
            ));
        if f.closure.is_some() {
            d = d.note("lambdas can't throw yet");
        }
        self.diags.push(d);
    }
}
