//! Type checking (spec §2.1–§2.2, §3.1, §5.1) over the HIR.
//!
//! Bidirectional: [`Checker::check`] pushes an expected type down (so `const x: u8 = 1` types
//! the literal as `u8`), [`Checker::infer`] computes a type bottom-up. Implicit conversions
//! happen only when lossless; they're recorded in [`Types::coercions`] for lowering.
//!
//! Return types are inferred on demand: a call to a function whose return type is still
//! unknown checks that function first. Reaching a function that is already being checked
//! means it's recursive, and recursive functions must declare their return type.

use crate::exhaust::{self, Ctor, Pat};
use crate::hir::*;
use crate::ty::{EnumId, FloatTy, IntTy, Ty};
use ppl_diag::{Diagnostic, Span, codes};
use ppl_syntax::ast::{BinOp, UnOp};
use ppl_syntax::token::NumSuffix;
use std::collections::{HashMap, HashSet};

#[derive(Clone, Debug, PartialEq)]
pub struct FnSig {
    pub params: Vec<Ty>,
    pub ret: Ty,
}

/// The type checker's results.
#[derive(Debug, Default)]
pub struct Types {
    /// The type of every expression (indexed by `ExprId`).
    pub exprs: Vec<Ty>,
    /// Expressions whose value is implicitly widened to another type.
    pub coercions: HashMap<ExprId, Ty>,
    /// Local types, per function.
    pub locals: Vec<Vec<Ty>>,
    pub globals: Vec<Ty>,
    pub sigs: Vec<FnSig>,
    /// Parsed format specs of template parts, keyed by the part's expression.
    pub formats: HashMap<ExprId, crate::format::FormatSpec>,
    /// Field index of each `object.field` (member expressions and assignment targets).
    pub fields: HashMap<ExprId, u32>,
    /// For each struct literal, the field index of each initializer, in source order.
    pub objects: HashMap<ExprId, (StructId, Vec<u32>)>,
    /// Method calls: the call expression and the method it calls.
    pub methods: HashMap<ExprId, FnId>,
    /// Variable reads of a `T?` binding that narrowing typed as `T` (lowering unwraps them).
    pub narrowed: HashSet<ExprId>,
    /// Variant patterns: the enum and variant index.
    pub variants: HashMap<ExprId, (EnumId, u32)>,
    /// Named field patterns (`Circle { r }`, `Point { x, y }`): each field's index, in order.
    pub pat_fields: HashMap<ExprId, Vec<u32>>,
    /// Variant constructors: the field index of each argument, in source order.
    pub variant_args: HashMap<ExprId, Vec<u32>>,
}

impl Types {
    pub fn expr(&self, id: ExprId) -> &Ty {
        &self.exprs[id.0 as usize]
    }

    /// The type an expression's value has after coercion.
    pub fn final_ty(&self, id: ExprId) -> &Ty {
        self.coercions
            .get(&id)
            .unwrap_or(&self.exprs[id.0 as usize])
    }
}

pub fn check(program: &Program) -> (Types, Vec<Diagnostic>) {
    let mut c = Checker {
        program,
        types: Types {
            exprs: vec![Ty::Error; program.expr_count()],
            coercions: HashMap::new(),
            locals: program
                .fns
                .iter()
                .map(|f| vec![Ty::Error; f.locals.len()])
                .collect(),
            globals: vec![Ty::Error; program.globals.len()],
            sigs: Vec::new(),
            formats: HashMap::new(),
            fields: HashMap::new(),
            objects: HashMap::new(),
            methods: HashMap::new(),
            narrowed: HashSet::new(),
            variants: HashMap::new(),
            pat_fields: HashMap::new(),
            variant_args: HashMap::new(),
        },
        narrow: Vec::new(),
        diags: Vec::new(),
        state: vec![FnState::Unchecked; program.fns.len()],
        global_known: vec![false; program.globals.len()],
        stack: Vec::new(),
    };
    for (i, g) in program.globals.iter().enumerate() {
        if let Some(t) = &g.ty {
            c.types.globals[i] = t.clone();
            c.global_known[i] = true;
        }
    }
    c.types.sigs = program
        .fns
        .iter()
        .map(|f| FnSig {
            params: f
                .params
                .iter()
                .map(|p| f.local(*p).ty.clone().unwrap_or(Ty::Error))
                .collect(),
            ret: f.ret.clone().unwrap_or(Ty::Error),
        })
        .collect();
    // The entry body first (it gives globals their types), then everything else.
    c.check_fn(program.entry);
    for i in 0..program.fns.len() {
        c.check_fn(FnId(i as u32));
    }
    (c.types, c.diags)
}

#[derive(Clone, Copy, PartialEq)]
enum FnState {
    Unchecked,
    InProgress,
    Done,
}

/// An expected type, with the span that imposed it (for "expected because of this" labels).
#[derive(Clone)]
struct Expect {
    ty: Ty,
    origin: Option<Span>,
}

impl Expect {
    fn new(ty: Ty) -> Expect {
        Expect { ty, origin: None }
    }

    fn from(ty: Ty, origin: Option<Span>) -> Expect {
        Expect { ty, origin }
    }
}

enum Ret {
    Declared(Ty, Option<Span>),
    /// Return types seen so far, for inference.
    Infer(Vec<(Ty, Span)>),
}

struct LoopCtx {
    /// `loop { }` (can produce a value) as opposed to `while`/`for`.
    produces_value: bool,
    want: Option<Expect>,
    breaks: Vec<(Ty, Span)>,
}

struct FnCtx {
    id: FnId,
    ret: Ret,
    loops: Vec<LoopCtx>,
}

struct Checker<'p> {
    program: &'p Program,
    types: Types,
    diags: Vec<Diagnostic>,
    state: Vec<FnState>,
    global_known: Vec<bool>,
    stack: Vec<FnCtx>,
    /// Flow-sensitive narrowing: scopes mapping variables to the type they're known to have
    /// (`T` for a `T?` binding after `x != null`).
    narrow: Vec<HashMap<Var, Ty>>,
}

/// What a condition tells about variables when it's true and when it's false.
#[derive(Default)]
struct Facts {
    when_true: Vec<(Var, Ty)>,
    when_false: Vec<(Var, Ty)>,
}

/// An integer literal without a suffix (possibly negated) adapts to its context.
fn untyped_lit(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::Int(_, None) | ExprKind::Float(_, None) => true,
        ExprKind::Unary(UnOp::Neg | UnOp::Plus, inner) => untyped_lit(inner),
        _ => false,
    }
}

const NULL_HELP: &str = "the value may be null: check it first (`if (x != null) …`), give a default with `??`, or assert with `!`";

fn is_float_lit(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::Float(..) => true,
        ExprKind::Unary(UnOp::Neg | UnOp::Plus, inner) => is_float_lit(inner),
        _ => false,
    }
}

fn suffix_ty(s: NumSuffix) -> Ty {
    match s {
        NumSuffix::I8 => Ty::Int(IntTy::I8),
        NumSuffix::I16 => Ty::Int(IntTy::I16),
        NumSuffix::I32 => Ty::Int(IntTy::I32),
        NumSuffix::I64 => Ty::Int(IntTy::I64),
        NumSuffix::U8 => Ty::Int(IntTy::U8),
        NumSuffix::U16 => Ty::Int(IntTy::U16),
        NumSuffix::U32 => Ty::Int(IntTy::U32),
        NumSuffix::U64 => Ty::Int(IntTy::U64),
        NumSuffix::Isize => Ty::Int(IntTy::Isize),
        NumSuffix::Usize => Ty::Int(IntTy::Usize),
        NumSuffix::F32 => Ty::Float(FloatTy::F32),
        NumSuffix::F64 => Ty::Float(FloatTy::F64),
    }
}

fn is_printable(t: &Ty) -> bool {
    match t {
        Ty::Nullable(inner) => is_printable(inner),
        _ => matches!(
            t,
            Ty::Int(_) | Ty::Float(_) | Ty::Bool | Ty::Char | Ty::Str | Ty::Error | Ty::Never
        ),
    }
}

impl<'p> Checker<'p> {
    fn ctx(&mut self) -> &mut FnCtx {
        self.stack.last_mut().expect("inside a function")
    }

    fn func(&self) -> &'p Function {
        self.program
            .func(self.stack.last().expect("inside a function").id)
    }

    fn set(&mut self, e: &Expr, t: Ty) -> Ty {
        self.types.exprs[e.id.0 as usize] = t.clone();
        t
    }

    fn mismatch(&mut self, span: Span, want: &Expect, found: &Ty) {
        if want.ty.is_error() || found.is_error() {
            return;
        }
        let mut d = Diagnostic::error(codes::MISMATCHED_TYPES, "mismatched types")
            .primary(span, format!("expected `{}`, found `{found}`", want.ty));
        if let Some(o) = want.origin {
            d = d.secondary(o, "expected because of this");
        }
        if want.ty.is_numeric() && found.is_numeric() {
            d = d.help(format!(
                "`{found}` doesn't convert to `{}` without possible loss; convert explicitly with `as {}`",
                want.ty, want.ty
            ));
        }
        if let Ty::Nullable(inner) = found
            && inner.widens_to(&want.ty)
        {
            d = d.help(NULL_HELP);
        }
        self.diags.push(d);
    }

    /// Accepts `found` where `want` is expected, recording a widening if needed.
    fn coerce(&mut self, e: &Expr, found: Ty, want: &Expect) -> Ty {
        if found == want.ty || found == Ty::Never || found.is_error() {
            return want.ty.clone();
        }
        if found.widens_to(&want.ty) {
            self.types.coercions.insert(e.id, want.ty.clone());
            return want.ty.clone();
        }
        self.mismatch(e.span, want, &found);
        Ty::Error
    }

    fn check(&mut self, e: &Expr, want: &Expect) -> Ty {
        let found = self.infer_with(e, Some(want));
        self.coerce(e, found, want)
    }

    fn infer(&mut self, e: &Expr) -> Ty {
        self.infer_with(e, None)
    }

    // ------------------------------------------------------------ narrowing

    fn narrowed(&self, v: Var) -> Option<&Ty> {
        self.narrow.iter().rev().find_map(|s| s.get(&v))
    }

    /// Runs `f` with `facts` known.
    fn with_facts<T>(&mut self, facts: Vec<(Var, Ty)>, f: impl FnOnce(&mut Self) -> T) -> T {
        self.narrow.push(facts.into_iter().collect());
        let r = f(self);
        self.narrow.pop();
        r
    }

    /// An assignment ends every narrowing of `v`.
    fn forget(&mut self, v: Var) {
        for s in &mut self.narrow {
            s.remove(&v);
        }
    }

    /// Before a loop: narrowings of variables the loop assigns don't hold on later iterations.
    fn forget_assigned(&mut self, exprs: &[&Expr]) {
        let mut vars = Vec::new();
        for e in exprs {
            e.assigned_vars(&mut vars);
        }
        for v in vars {
            self.forget(v);
        }
    }

    /// The facts of an already checked condition.
    fn facts(&self, cond: &Expr) -> Facts {
        match &cond.kind {
            ExprKind::Unary(UnOp::Not, inner) => {
                let f = self.facts(inner);
                Facts {
                    when_true: f.when_false,
                    when_false: f.when_true,
                }
            }
            ExprKind::Binary(op @ (BinOp::Eq | BinOp::Ne), l, r) => {
                let var = match (&l.kind, &r.kind) {
                    (ExprKind::Var(v), ExprKind::Null) => Some((*v, l)),
                    (ExprKind::Null, ExprKind::Var(v)) => Some((*v, r)),
                    _ => None,
                };
                let Some((v, ve)) = var else {
                    return Facts::default();
                };
                let Ty::Nullable(inner) = self.types.expr(ve.id) else {
                    return Facts::default();
                };
                let fact = vec![(v, (**inner).clone())];
                if *op == BinOp::Ne {
                    Facts {
                        when_true: fact,
                        when_false: Vec::new(),
                    }
                } else {
                    Facts {
                        when_true: Vec::new(),
                        when_false: fact,
                    }
                }
            }
            // `x is Circle { .. }` on a `Shape?` says `x` isn't null; `x is null` the opposite.
            ExprKind::Is { expr, pat } => {
                let ExprKind::Var(v) = expr.kind else {
                    return Facts::default();
                };
                let vt = self.types.expr(expr.id).clone();
                let Ty::Nullable(inner) = &vt else {
                    return Facts::default();
                };
                let fact = vec![(v, (**inner).clone())];
                if pat.looks_inside(&vt) {
                    Facts {
                        when_true: fact,
                        when_false: Vec::new(),
                    }
                } else if matches!(pat.kind, PatKind::Lit(LitPat::Null)) {
                    Facts {
                        when_true: Vec::new(),
                        when_false: fact,
                    }
                } else {
                    Facts::default()
                }
            }
            ExprKind::Binary(BinOp::And, l, r) => {
                let (a, b) = (self.facts(l), self.facts(r));
                Facts {
                    when_true: a.when_true.into_iter().chain(b.when_true).collect(),
                    when_false: Vec::new(),
                }
            }
            ExprKind::Binary(BinOp::Or, l, r) => {
                let (a, b) = (self.facts(l), self.facts(r));
                Facts {
                    when_true: Vec::new(),
                    when_false: a.when_false.into_iter().chain(b.when_false).collect(),
                }
            }
            _ => Facts::default(),
        }
    }

    // ------------------------------------------------------------ functions

    fn check_fn(&mut self, id: FnId) {
        match self.state[id.0 as usize] {
            FnState::Done => return,
            FnState::InProgress => return,
            FnState::Unchecked => {}
        }
        self.state[id.0 as usize] = FnState::InProgress;
        let f = self.program.func(id);
        for (i, l) in f.locals.iter().enumerate() {
            if let Some(t) = &l.ty {
                self.types.locals[id.0 as usize][i] = t.clone();
            }
        }
        let ret = match &f.ret {
            Some(t) => Ret::Declared(t.clone(), f.ret_span),
            None => Ret::Infer(Vec::new()),
        };
        self.stack.push(FnCtx {
            id,
            ret,
            loops: Vec::new(),
        });
        let body_ty = match (&f.ret, f.is_entry) {
            (_, true) | (Some(Ty::Void), _) => {
                // The value of the last expression is discarded.
                self.block(&f.body, None)
            }
            (Some(t), false) => {
                let want = Expect::from(t.clone(), f.ret_span);
                let bt = self.block(&f.body, Some(&want));
                if f.body.tail.is_none() && bt != Ty::Never && *t != Ty::Void && !t.is_error() {
                    self.diags.push(
                        Diagnostic::error(codes::MISMATCHED_TYPES, "mismatched types")
                            .primary(
                                f.body.span,
                                format!("expected `{t}`, but the body ends without a value"),
                            )
                            .secondary(
                                f.ret_span.unwrap_or(f.name_span),
                                "the return type is declared here",
                            )
                            .help("make the last expression the result, or add a `return`"),
                    );
                }
                bt
            }
            (None, false) => self.block(&f.body, None),
        };
        let ctx = self.stack.pop().unwrap();
        if let Ret::Infer(mut rets) = ctx.ret {
            rets.push((
                body_ty,
                f.body.tail.as_ref().map_or(f.body.span, |t| t.span),
            ));
            let ret = self.unify_all(&rets, "the function returns values of different types");
            self.types.sigs[id.0 as usize].ret = ret;
        }
        self.state[id.0 as usize] = FnState::Done;
    }

    /// Joins several types: identical, or all widening to one of them. `never` disappears.
    fn unify_all(&mut self, tys: &[(Ty, Span)], msg: &str) -> Ty {
        let live: Vec<&(Ty, Span)> = tys.iter().filter(|(t, _)| *t != Ty::Never).collect();
        let Some(first) = live.first() else {
            return Ty::Never;
        };
        let mut acc = first.0.clone();
        for (t, _) in &live[1..] {
            if t.widens_to(&acc) {
                continue;
            }
            if acc.widens_to(t) {
                acc = t.clone();
                continue;
            }
            if t.is_error() || acc.is_error() {
                return Ty::Error;
            }
            let mut d = Diagnostic::error(codes::MISMATCHED_TYPES, msg.to_string());
            for (t, s) in &live {
                d = d.secondary(*s, format!("`{t}`"));
            }
            d.labels[0].primary = true;
            self.diags.push(d);
            return Ty::Error;
        }
        acc
    }

    // ------------------------------------------------------------ blocks and statements

    fn block(&mut self, b: &Block, want: Option<&Expect>) -> Ty {
        self.narrow.push(HashMap::new());
        let mut diverges = false;
        for s in &b.stmts {
            match s {
                Stmt::Let { var, init, span } => self.let_stmt(*var, init.as_ref(), *span),
                Stmt::Expr(e) => {
                    if self.infer(e) == Ty::Never {
                        diverges = true;
                    }
                    self.early_exit_facts(e);
                }
            }
        }
        let t = match &b.tail {
            Some(t) => match want {
                Some(w) => self.check(t, w),
                None => self.infer(t),
            },
            None if diverges => Ty::Never,
            None => Ty::Void,
        };
        self.narrow.pop();
        t
    }

    /// `if (x == null) return` narrows `x` for the rest of the block (and `if (c) … else
    /// return` keeps what `c` says).
    fn early_exit_facts(&mut self, e: &Expr) {
        let ExprKind::If { cond, then, else_ } = &e.kind else {
            return;
        };
        let then_exits = *self.types.expr(then.id) == Ty::Never;
        let else_exits = else_
            .as_ref()
            .is_some_and(|x| *self.types.expr(x.id) == Ty::Never);
        let facts = self.facts(cond);
        let known = match (then_exits, else_exits) {
            (true, false) => facts.when_false,
            (false, true) => facts.when_true,
            _ => return,
        };
        let scope = self.narrow.last_mut().expect("block scope");
        scope.extend(known);
    }

    fn var_binding(&self, var: Var) -> &'p Binding {
        match var {
            Var::Local(l) => self.func().local(l),
            Var::Global(g) => self.program.global(g),
        }
    }

    fn var_ty(&mut self, var: Var, span: Span) -> Ty {
        match var {
            Var::Local(l) => {
                let f = self.stack.last().unwrap().id;
                self.types.locals[f.0 as usize][l.0 as usize].clone()
            }
            Var::Global(g) => {
                if !self.global_known[g.0 as usize] {
                    let b = self.program.global(g);
                    self.diags.push(
                        Diagnostic::error(
                            codes::MISMATCHED_TYPES,
                            format!("the type of `{}` isn't known here yet", b.name),
                        )
                        .primary(
                            span,
                            "used before the top-level code reaches its declaration",
                        )
                        .secondary(b.span, "declared here")
                        .help(format!("add a type annotation: `{}: T`", b.name)),
                    );
                    self.global_known[g.0 as usize] = true;
                }
                self.types.globals[g.0 as usize].clone()
            }
        }
    }

    fn set_var_ty(&mut self, var: Var, t: Ty) {
        match var {
            Var::Local(l) => {
                let f = self.stack.last().unwrap().id;
                self.types.locals[f.0 as usize][l.0 as usize] = t;
            }
            Var::Global(g) => {
                self.types.globals[g.0 as usize] = t;
                self.global_known[g.0 as usize] = true;
            }
        }
    }

    fn let_stmt(&mut self, var: Var, init: Option<&Expr>, span: Span) {
        let b = self.var_binding(var);
        let t = match (&b.ty, init) {
            (Some(ann), Some(init)) => {
                let origin = Some(b.span);
                self.check(init, &Expect::from(ann.clone(), origin));
                ann.clone()
            }
            (Some(ann), None) => ann.clone(),
            (None, Some(init)) => {
                let t = self.infer(init);
                match t {
                    Ty::Null => {
                        self.diags.push(
                            Diagnostic::error(
                                codes::MISMATCHED_TYPES,
                                "can't infer a type from `null`",
                            )
                            .primary(init.span, "only `null`")
                            .help(format!("declare the type: `{}: T? = null`", b.name)),
                        );
                        Ty::Error
                    }
                    Ty::Void => {
                        self.diags.push(
                            Diagnostic::error(
                                codes::MISMATCHED_TYPES,
                                "this expression has no value",
                            )
                            .primary(init.span, "has type `void`"),
                        );
                        Ty::Error
                    }
                    t => t,
                }
            }
            (None, None) => {
                self.diags.push(
                    Diagnostic::error(
                        codes::MISMATCHED_TYPES,
                        format!("`{}` needs a type or a value", b.name),
                    )
                    .primary(span, "neither a type annotation nor an initializer")
                    .help(format!("write `{}: T` or `{} = value`", b.name, b.name)),
                );
                Ty::Error
            }
        };
        self.set_var_ty(var, t);
    }

    // ------------------------------------------------------------ expressions

    fn infer_with(&mut self, e: &Expr, want: Option<&Expect>) -> Ty {
        let t = self.expr_kind(e, want);
        self.set(e, t)
    }

    fn expr_kind(&mut self, e: &Expr, want: Option<&Expect>) -> Ty {
        match &e.kind {
            ExprKind::Int(v, suffix) => self.int_lit(e, *v as i128, *suffix, want),
            ExprKind::Float(_, suffix) => match (suffix, want.map(|w| &w.ty)) {
                (Some(s), _) => suffix_ty(*s),
                (None, Some(Ty::Float(f))) => Ty::Float(*f),
                (None, Some(Ty::Nullable(inner))) if matches!(**inner, Ty::Float(_)) => {
                    (**inner).clone()
                }
                _ => Ty::NUMBER,
            },
            ExprKind::Str(s) => {
                let wants_char = matches!(want.map(|w| &w.ty), Some(Ty::Char));
                if wants_char {
                    if s.chars().count() == 1 {
                        return Ty::Char;
                    }
                    self.diags.push(
                        Diagnostic::error(codes::MISMATCHED_TYPES, "mismatched types")
                            .primary(
                                e.span,
                                format!(
                                    "expected `char`, found a string of {} characters",
                                    s.chars().count()
                                ),
                            )
                            .note(
                                "a `char` literal is a string literal with exactly one character",
                            ),
                    );
                    return Ty::Error;
                }
                Ty::Str
            }
            ExprKind::Bool(_) => Ty::Bool,
            ExprKind::Null => Ty::Null,
            ExprKind::Var(v) => {
                if let Some(t) = self.narrowed(*v).cloned() {
                    self.types.narrowed.insert(e.id);
                    return t;
                }
                self.var_ty(*v, e.span)
            }
            ExprKind::Fn(_) | ExprKind::Builtin(_) => {
                self.diags.push(
                    Diagnostic::error(
                        codes::NOT_IMPLEMENTED,
                        "functions as values aren't implemented yet",
                    )
                    .primary(e.span, "call it instead"),
                );
                Ty::Error
            }
            ExprKind::Unary(op, inner) => self.unary(e, *op, inner, want),
            ExprKind::Binary(op, l, r) => self.binary(e, *op, l, r, want),
            ExprKind::Assign { op, target, value } => {
                if let Target::Var(v) = target {
                    self.forget(*v);
                }
                let (vt, origin) = self.target_ty(target);
                match op {
                    None => {
                        self.check(value, &Expect::from(vt, origin));
                    }
                    Some(op) => {
                        self.compound(e, *op, vt, value);
                    }
                }
                Ty::Void
            }
            ExprKind::Object { ty, fields } => self.object(e, *ty, fields, want),
            ExprKind::IncDec { target, .. } => {
                if let Target::Var(v) = target {
                    self.forget(*v);
                }
                let (t, _) = self.target_ty(target);
                if !t.is_int() && !t.is_error() {
                    self.invalid_operand(e.span, "`++`/`--`", &t, "an integer");
                    return Ty::Error;
                }
                t
            }
            ExprKind::Call { callee, args } => self.call(e, callee, args),
            ExprKind::Member {
                object,
                name,
                name_span,
            } => {
                let ot = self.infer(object);
                match (&ot, name.as_str()) {
                    (Ty::Str | Ty::Array(_) | Ty::Fixed(..), "length") => Ty::INT,
                    (Ty::Range(it), "start" | "end") => Ty::Int(*it),
                    (Ty::Error, _) => Ty::Error,
                    (Ty::Struct(sid, _), _) => self.field(e.id, *sid, name, *name_span),
                    _ => {
                        let mut d = Diagnostic::error(
                            codes::UNKNOWN_MEMBER,
                            format!("`{ot}` has no member `{name}`"),
                        )
                        .primary(*name_span, "unknown member");
                        if let Ty::Nullable(_) = ot {
                            d = d.help(format!(
                                "the value may be null: use `?.{name}`, or check it with `!= null` first"
                            ));
                        }
                        self.diags.push(d);
                        Ty::Error
                    }
                }
            }
            ExprKind::Index { object, index } => {
                let ot = self.infer(object);
                match ot {
                    Ty::Str => {
                        self.check(index, &Expect::new(Ty::INT));
                        Ty::Char
                    }
                    Ty::Array(t) | Ty::Fixed(t, _) => {
                        self.check(index, &Expect::new(Ty::INT));
                        *t
                    }
                    Ty::Tuple(ts) => self.tuple_index(&ts, index),
                    Ty::Error => {
                        self.infer(index);
                        Ty::Error
                    }
                    _ => {
                        self.infer(index);
                        self.diags.push(
                            Diagnostic::error(
                                codes::INVALID_OPERANDS,
                                format!("`{ot}` can't be indexed"),
                            )
                            .primary(object.span, format!("has type `{ot}`")),
                        );
                        Ty::Error
                    }
                }
            }
            ExprKind::Template { parts, .. } => {
                for p in parts {
                    let t = self.infer(&p.expr);
                    let compound = crate::format::is_compound(&t);
                    let debug = compound && p.spec.as_deref().is_some_and(|s| s.ends_with('?'));
                    if !is_printable(&t) && !debug {
                        if compound {
                            self.diags.push(
                                Diagnostic::error(
                                    codes::INVALID_OPERANDS,
                                    format!("`{t}` has no string form"),
                                )
                                .primary(p.expr.span, "a compound value in a template")
                                .help("show its fields with the debug format: `${value:?}`"),
                            );
                            continue;
                        }
                        self.diags.push(
                            Diagnostic::error(
                                codes::INVALID_OPERANDS,
                                format!("`{t}` can't be put in a template"),
                            )
                            .primary(p.expr.span, "has no string form"),
                        );
                        continue;
                    }
                    if let Some(spec) = &p.spec {
                        self.format_spec(&p.expr, spec);
                    }
                }
                Ty::Str
            }
            ExprKind::If { cond, then, else_ } => self.if_expr(cond, then, else_.as_deref(), want),
            ExprKind::Block(b) => self.block(b, want),
            ExprKind::Loop(b) => {
                self.forget_assigned(&[e]);
                let want = want.filter(|w| w.ty != Ty::Void).cloned();
                self.ctx().loops.push(LoopCtx {
                    produces_value: true,
                    want: want.clone(),
                    breaks: Vec::new(),
                });
                self.block(b, None);
                let breaks = self.ctx().loops.pop().unwrap().breaks;
                if breaks.is_empty() {
                    return Ty::Never;
                }
                if let Some(w) = want {
                    // Break values were already checked against the expectation.
                    return w.ty;
                }
                self.unify_all(&breaks, "`break` values have different types")
            }
            ExprKind::While {
                cond, body, update, ..
            } => {
                self.forget_assigned(&[e]);
                self.check(cond, &Expect::new(Ty::Bool));
                let facts = self.facts(cond);
                self.ctx().loops.push(LoopCtx {
                    produces_value: false,
                    want: None,
                    breaks: Vec::new(),
                });
                self.with_facts(facts.when_true, |c| {
                    c.infer(body);
                    if let Some(u) = update {
                        c.infer(u);
                    }
                });
                self.ctx().loops.pop();
                Ty::Void
            }
            ExprKind::Break(value) => {
                // The innermost loop; `want` of an enclosing `loop` is passed via break types.
                let want = self.ctx().loops.last().and_then(|l| l.want.clone());
                let t = match (value, &want) {
                    (Some(v), Some(w)) => self.check(v, w),
                    (Some(v), None) => self.infer(v),
                    (None, _) => Ty::Void,
                };
                if let Some(l) = self.ctx().loops.last_mut()
                    && l.produces_value
                {
                    l.breaks
                        .push((t, value.as_ref().map_or(e.span, |v| v.span)));
                }
                Ty::Never
            }
            ExprKind::Continue => Ty::Never,
            ExprKind::Return(value) => {
                self.ret(e, value.as_deref());
                Ty::Never
            }
            ExprKind::As(inner, to) => {
                let from = self.infer(inner);
                let ok = match (&from, to) {
                    (a, b) if a == b => true,
                    (Ty::Int(_) | Ty::Float(_), Ty::Int(_) | Ty::Float(_)) => true,
                    (Ty::Char, Ty::Int(_)) => true,
                    (Ty::Int(IntTy::U8 | IntTy::U32), Ty::Char) => true,
                    (Ty::Error, _) | (_, Ty::Error) => true,
                    _ => false,
                };
                if !ok {
                    self.diags.push(
                        Diagnostic::error(
                            codes::INVALID_CAST,
                            format!("can't convert `{from}` to `{to}` with `as`"),
                        )
                        .primary(e.span, "invalid conversion")
                        .note("`as` converts between numeric types (and `char` ↔ integer codes)"),
                    );
                    return Ty::Error;
                }
                to.clone()
            }
            ExprKind::Array(elems) => self.array_lit(e, elems, want),
            ExprKind::Repeat { value, count } => self.repeat_lit(e, value, *count, want),
            ExprKind::Range { start, end } => self.range(e, start, end, want),
            ExprKind::ForOf { var, iter, body } => {
                self.forget_assigned(&[body]);
                let it = self.infer(iter);
                let elem = match &it {
                    Ty::Range(i) => Ty::Int(*i),
                    Ty::Array(t) | Ty::Fixed(t, _) => (**t).clone(),
                    Ty::Str => Ty::Char,
                    Ty::Error => Ty::Error,
                    other => {
                        self.diags.push(
                            Diagnostic::error(
                                codes::INVALID_OPERANDS,
                                format!("`{other}` isn't iterable"),
                            )
                            .primary(iter.span, format!("has type `{other}`"))
                            .note("`for-of` works over arrays, fixed arrays, ranges and strings"),
                        );
                        Ty::Error
                    }
                };
                self.set_var_ty(Var::Local(*var), elem);
                self.ctx().loops.push(LoopCtx {
                    produces_value: false,
                    want: None,
                    breaks: Vec::new(),
                });
                self.infer(body);
                self.ctx().loops.pop();
                Ty::Void
            }
            ExprKind::Variant {
                enum_,
                variant,
                args,
            } => self.variant(e, *enum_, *variant, args.as_deref()),
            ExprKind::Match {
                scrutinee,
                arms,
                switch,
            } => self.match_expr(e, scrutinee, arms, *switch, want),
            ExprKind::Is { expr, pat } => {
                let t = self.infer(expr);
                self.check_pattern(pat, &t);
                Ty::Bool
            }
            ExprKind::NonNull(inner) => match self.infer(inner) {
                Ty::Nullable(t) => *t,
                Ty::Null => {
                    self.diags.push(
                        Diagnostic::error(codes::INVALID_OPERANDS, "`null!` always panics")
                            .primary(e.span, "this value is always null"),
                    );
                    Ty::Error
                }
                t => t,
            },
            ExprKind::OptChain { tmp, base, rest } => {
                let bt = self.infer(base);
                let (inner, nullable) = match bt {
                    Ty::Nullable(t) => (*t, true),
                    t => (t, false),
                };
                self.set_var_ty(Var::Local(*tmp), inner);
                let rt = self.infer(rest);
                match rt {
                    Ty::Void | Ty::Never | Ty::Error => rt,
                    t if nullable => t.nullable(),
                    t => t,
                }
            }
            ExprKind::Error => Ty::Error,
        }
    }

    /// `Shape.Circle(r: 1)`, `Shape.Rect(2, 3)`, `Color.Red`.
    fn variant(&mut self, e: &Expr, eid: EnumId, vi: u32, args: Option<&[VariantArg]>) -> Ty {
        let def = self.program.enum_def(eid);
        let v = &def.variants[vi as usize];
        let ty = self.program.enum_ty(eid);
        let Some(args) = args else {
            if !v.fields.is_empty() || v.has_parens {
                let fields: Vec<String> = v
                    .fields
                    .iter()
                    .map(|f| format!("{}: {}", f.name, f.ty))
                    .collect();
                self.diags.push(
                    Diagnostic::error(
                        codes::WRONG_ARG_COUNT,
                        format!("`{}.{}` needs its fields", def.name, v.name),
                    )
                    .primary(e.span, "no arguments given")
                    .secondary(
                        v.span,
                        format!("declared as `{}({})`", v.name, fields.join(", ")),
                    ),
                );
            }
            return ty;
        };
        if !v.has_parens {
            self.diags.push(
                Diagnostic::error(
                    codes::WRONG_ARG_COUNT,
                    format!("`{}.{}` has no fields", def.name, v.name),
                )
                .primary(e.span, "called with parentheses")
                .help(format!("write `{}.{}`", def.name, v.name)),
            );
            for a in args {
                self.infer(&a.value);
            }
            return ty;
        }
        let mut seen: Vec<Option<Span>> = vec![None; v.fields.len()];
        let mut order = Vec::new();
        let mut next_positional = 0;
        let mut named_seen = false;
        for a in args {
            let idx = match &a.name {
                None => {
                    if named_seen {
                        self.diags.push(
                            Diagnostic::error(
                                codes::POSITIONAL_AFTER_NAMED,
                                "positional argument after a named one",
                            )
                            .primary(a.value.span, "move it before the named arguments"),
                        );
                    }
                    let i = next_positional;
                    next_positional += 1;
                    if i >= v.fields.len() {
                        self.diags.push(
                            Diagnostic::error(
                                codes::WRONG_ARG_COUNT,
                                format!(
                                    "`{}.{}` takes {} argument{}",
                                    def.name,
                                    v.name,
                                    v.fields.len(),
                                    if v.fields.len() == 1 { "" } else { "s" }
                                ),
                            )
                            .primary(a.value.span, "extra argument"),
                        );
                        None
                    } else {
                        Some(i)
                    }
                }
                Some((name, span)) => {
                    named_seen = true;
                    match v.field(name) {
                        Some((i, _)) => Some(i),
                        None => {
                            self.diags.push(
                                Diagnostic::error(
                                    codes::UNKNOWN_MEMBER,
                                    format!("`{}.{}` has no field `{name}`", def.name, v.name),
                                )
                                .primary(*span, "unknown field")
                                .secondary(v.span, "variant declared here"),
                            );
                            None
                        }
                    }
                }
            };
            match idx {
                Some(i) => {
                    let span = a.name.as_ref().map_or(a.value.span, |(_, s)| *s);
                    if let Some(prev) = seen[i] {
                        self.diags.push(
                            Diagnostic::error(
                                codes::DUPLICATE_FIELD,
                                format!("field `{}` is given twice", v.fields[i].name),
                            )
                            .primary(span, "given again here")
                            .secondary(prev, "first given here"),
                        );
                    }
                    seen[i] = Some(span);
                    order.push(i as u32);
                    let f = &v.fields[i];
                    self.check(&a.value, &Expect::from(f.ty.clone(), Some(f.span)));
                }
                None => {
                    order.push(u32::MAX);
                    self.infer(&a.value);
                }
            }
        }
        let missing: Vec<String> = v
            .fields
            .iter()
            .zip(&seen)
            .filter(|(_, s)| s.is_none())
            .map(|(f, _)| format!("`{}`", f.name))
            .collect();
        if !missing.is_empty() {
            self.diags.push(
                Diagnostic::error(
                    codes::MISSING_FIELDS,
                    format!(
                        "missing field{} {}",
                        if missing.len() == 1 { "" } else { "s" },
                        missing.join(", ")
                    ),
                )
                .primary(
                    e.span,
                    format!("`{}.{}` needs every field", def.name, v.name),
                )
                .secondary(v.span, "variant declared here"),
            );
        }
        self.types.variant_args.insert(e.id, order);
        ty
    }

    fn match_expr(
        &mut self,
        e: &Expr,
        scrutinee: &Expr,
        arms: &[Arm],
        switch: Option<bool>,
        want: Option<&Expect>,
    ) -> Ty {
        let st = self.infer(scrutinee);
        let want = want.filter(|w| w.ty != Ty::Void);
        // A switch statement needn't cover every case (and has no value).
        let statement = switch == Some(false) && want.is_none();
        let mut results: Vec<(&Expr, Ty)> = Vec::new();
        for arm in arms {
            self.check_pattern(&arm.pat, &st);
            if let Some(g) = &arm.guard {
                self.check(g, &Expect::new(Ty::Bool));
            }
            let facts = arm
                .guard
                .as_ref()
                .map(|g| self.facts(g).when_true)
                .unwrap_or_default();
            let t = self.with_facts(facts, |c| match want {
                Some(w) => c.check(&arm.body, w),
                None => c.infer(&arm.body),
            });
            results.push((&arm.body, t));
        }
        if !st.is_error() && !statement {
            let rows: Vec<Pat> = arms
                .iter()
                .filter(|a| a.guard.is_none())
                .flat_map(|a| self.to_pat(&a.pat, &st))
                .collect();
            let ex = exhaust::Checker {
                program: self.program,
            };
            let missing = ex.missing(&st, &rows, 8);
            if !missing.is_empty() {
                let list: Vec<String> = missing.iter().map(|m| format!("`{m}`")).collect();
                self.diags.push(
                    Diagnostic::error(
                        codes::NON_EXHAUSTIVE,
                        format!("non-exhaustive match: missing {}", list.join(", ")),
                    )
                    .primary(
                        scrutinee.span,
                        format!("`{st}` isn't covered by every case"),
                    )
                    .help("add arms for the missing cases, or a wildcard arm `_ => …`")
                    .note("arms with an `if` guard don't count, since the guard can be false"),
                );
            }
        }
        let _ = e;
        if statement {
            return Ty::Void;
        }
        if let Some(w) = want {
            return w.ty.clone();
        }
        self.unify_exprs(&results, "`match` arms have different types")
    }

    /// Joins the types of several expressions (match arms), recording widenings, and turning
    /// `T` + `null` into `T?`.
    fn unify_exprs(&mut self, items: &[(&Expr, Ty)], msg: &str) -> Ty {
        let live: Vec<&(&Expr, Ty)> = items.iter().filter(|(_, t)| *t != Ty::Never).collect();
        let Some(first) = live.first() else {
            return Ty::Never;
        };
        if live.iter().any(|(_, t)| t.is_error()) {
            return Ty::Error;
        }
        let mut acc = first.1.clone();
        for (_, t) in &live[1..] {
            if t.widens_to(&acc) {
                continue;
            }
            if acc.widens_to(t) {
                acc = t.clone();
                continue;
            }
            if *t == Ty::Null && acc != Ty::Void {
                acc = acc.nullable();
                continue;
            }
            if acc == Ty::Null && *t != Ty::Void {
                acc = t.clone().nullable();
                continue;
            }
            let mut d = Diagnostic::error(codes::MISMATCHED_TYPES, msg.to_string());
            for (x, t) in &live {
                d = d.secondary(x.span, format!("`{t}`"));
            }
            d.labels[0].primary = true;
            self.diags.push(d);
            return Ty::Error;
        }
        for (x, t) in &live {
            if *t != acc {
                self.types.coercions.insert(x.id, acc.clone());
            }
        }
        acc
    }

    fn pattern_mismatch(&mut self, span: Span, ty: &Ty, what: &str) {
        if ty.is_error() {
            return;
        }
        self.diags.push(
            Diagnostic::error(
                codes::PATTERN_MISMATCH,
                format!("this pattern can't match a `{ty}`"),
            )
            .primary(span, format!("{what} pattern"))
            .note(format!("the matched value has type `{ty}`")),
        );
    }

    /// Checks a pattern against the type of the value it matches, typing its bindings.
    fn check_pattern(&mut self, p: &Pattern, ty: &Ty) {
        self.types.exprs[p.id.0 as usize] = ty.clone();
        let inner = match ty {
            Ty::Nullable(t) if p.looks_inside(ty) => (**t).clone(),
            _ => ty.clone(),
        };
        match &p.kind {
            PatKind::Wild => {}
            PatKind::Bind(l) => self.bind_local(*l, ty, p.span),
            PatKind::Or(alts) => {
                for a in alts {
                    self.check_pattern(a, ty);
                }
            }
            PatKind::As(l, sub) => {
                self.bind_local(*l, &inner, p.span);
                self.check_pattern(sub, ty);
            }
            PatKind::Range(lo, hi) => {
                for l in [lo, hi] {
                    let ok = match (l, &inner) {
                        (LitPat::Int(v), Ty::Int(it)) => {
                            if !it.contains(*v) && *v != it.max() + 1 {
                                self.lit_out_of_range(p.span, *v, *it);
                            }
                            true
                        }
                        (LitPat::Str(s), Ty::Char) => s.chars().count() == 1,
                        (_, Ty::Error) => true,
                        _ => false,
                    };
                    if !ok {
                        self.pattern_mismatch(p.span, ty, "a range");
                        break;
                    }
                }
            }
            PatKind::Slice {
                prefix,
                rest,
                suffix,
            } => self.slice_pattern(p, ty, &inner, prefix, *rest, suffix),
            PatKind::Type {
                ty: want,
                bind,
                fields,
            } => {
                // Until unions (P3.12), a type test either always holds or checks for null.
                let ok = *want == inner || want.is_error() || inner.is_error();
                if !ok {
                    self.diags.push(
                        Diagnostic::error(
                            codes::PATTERN_MISMATCH,
                            format!("a `{ty}` is never a `{want}`"),
                        )
                        .primary(p.span, "this type test can't succeed"),
                    );
                }
                let bt = if ok { want.clone() } else { Ty::Error };
                if let Some(l) = bind {
                    self.bind_local(*l, &bt, p.span);
                }
                if let Some(fps) = fields {
                    match &bt {
                        Ty::Struct(sid, _) => {
                            let def = self.program.struct_def(*sid);
                            let field_tys: Vec<(String, Ty)> = def
                                .fields
                                .iter()
                                .map(|f| (f.name.clone(), f.ty.clone()))
                                .collect();
                            let what = format!("`{}`", def.name);
                            self.field_patterns(p, fps, &field_tys, &what);
                        }
                        _ => {
                            for f in fps {
                                self.check_pattern(&f.pat, &Ty::Error);
                            }
                            if !bt.is_error() {
                                self.pattern_mismatch(p.span, ty, "a field");
                            }
                        }
                    }
                }
            }
            PatKind::Lit(l) => {
                let ok = match (l, &inner) {
                    (LitPat::Int(v), Ty::Int(it)) => {
                        if !it.contains(*v) {
                            self.lit_out_of_range(p.span, *v, *it);
                        }
                        true
                    }
                    (LitPat::Int(_) | LitPat::Float(_), Ty::Float(_)) => true,
                    (LitPat::Str(_), Ty::Str) => true,
                    (LitPat::Str(s), Ty::Char) => s.chars().count() == 1,
                    (LitPat::Bool(_), Ty::Bool) => true,
                    (LitPat::Null, Ty::Nullable(_)) => true,
                    (_, Ty::Error) => true,
                    _ => false,
                };
                if !ok {
                    self.pattern_mismatch(p.span, ty, "a literal");
                }
            }
            PatKind::Variant {
                enum_,
                name,
                name_span,
                args,
            } => {
                let eid = match &inner {
                    Ty::Enum(id, _) => *id,
                    Ty::Error => return self.wild_subpatterns(args),
                    _ => {
                        self.pattern_mismatch(p.span, ty, "a variant");
                        return self.wild_subpatterns(args);
                    }
                };
                if let Some(named) = enum_
                    && *named != eid
                {
                    self.pattern_mismatch(
                        p.span,
                        ty,
                        &format!("a `{}`", self.program.enum_def(*named).name),
                    );
                    return self.wild_subpatterns(args);
                }
                let def = self.program.enum_def(eid);
                let Some((vi, v)) = def.variant(name) else {
                    let names: Vec<&str> = def.variants.iter().map(|v| v.name.as_str()).collect();
                    self.diags.push(
                        Diagnostic::error(
                            codes::UNKNOWN_MEMBER,
                            format!("enum `{}` has no variant `{name}`", def.name),
                        )
                        .primary(*name_span, "unknown variant")
                        .note(format!("its variants are: {}", names.join(", "))),
                    );
                    return self.wild_subpatterns(args);
                };
                self.types.variants.insert(p.id, (eid, vi));
                let field_tys: Vec<(String, Ty)> = v
                    .fields
                    .iter()
                    .map(|f| (f.name.clone(), f.ty.clone()))
                    .collect();
                match args {
                    VariantPatArgs::None => {}
                    VariantPatArgs::Positional(ps) => {
                        if ps.len() != field_tys.len() {
                            self.diags.push(
                                Diagnostic::error(
                                    codes::WRONG_LENGTH,
                                    format!(
                                        "`{name}` has {} field{}, but the pattern has {}",
                                        field_tys.len(),
                                        if field_tys.len() == 1 { "" } else { "s" },
                                        ps.len()
                                    ),
                                )
                                .primary(p.span, "wrong number of fields")
                                .help("use `_` for fields you don't need"),
                            );
                        }
                        for (i, sub) in ps.iter().enumerate() {
                            match field_tys.get(i) {
                                Some((_, t)) => self.check_pattern(sub, t),
                                None => self.check_pattern(sub, &Ty::Error),
                            }
                        }
                    }
                    VariantPatArgs::Named(fps) => {
                        let what = format!("`{name}`");
                        self.field_patterns(p, fps, &field_tys, &what);
                    }
                }
            }
            PatKind::Struct { sid, fields } => {
                let st = self.program.struct_ty(*sid);
                if inner != st && !inner.is_error() {
                    self.pattern_mismatch(p.span, ty, &format!("a `{st}`"));
                    for f in fields {
                        self.check_pattern(&f.pat, &Ty::Error);
                    }
                    return;
                }
                let def = self.program.struct_def(*sid);
                let field_tys: Vec<(String, Ty)> = def
                    .fields
                    .iter()
                    .map(|f| (f.name.clone(), f.ty.clone()))
                    .collect();
                let what = format!("`{}`", def.name);
                self.field_patterns(p, fields, &field_tys, &what);
            }
        }
    }

    /// `[a, b]`, `[first, ...rest]` on a tuple, fixed array or growable array.
    fn slice_pattern(
        &mut self,
        p: &Pattern,
        ty: &Ty,
        inner: &Ty,
        prefix: &[Pattern],
        rest: Option<Option<LocalId>>,
        suffix: &[Pattern],
    ) {
        let n = prefix.len() + suffix.len();
        let subs: Vec<&Pattern> = prefix.iter().chain(suffix).collect();
        let fail = |c: &mut Self| {
            for sub in &subs {
                c.check_pattern(sub, &Ty::Error);
            }
            if let Some(Some(l)) = rest {
                c.bind_local(l, &Ty::Error, p.span);
            }
        };
        match inner {
            Ty::Array(t) => {
                for sub in &subs {
                    self.check_pattern(sub, t);
                }
                if let Some(Some(l)) = rest {
                    self.bind_local(l, inner, p.span);
                }
            }
            Ty::Tuple(ts) if rest.is_none() && ts.len() == n => {
                for (sub, t) in subs.iter().zip(ts) {
                    self.check_pattern(sub, t);
                }
            }
            Ty::Fixed(t, len) if rest.is_none() && *len as usize == n => {
                for sub in &subs {
                    self.check_pattern(sub, t);
                }
            }
            Ty::Tuple(_) | Ty::Fixed(..) => {
                let len = match inner {
                    Ty::Tuple(ts) => ts.len(),
                    Ty::Fixed(_, n) => *n as usize,
                    _ => unreachable!(),
                };
                let msg = if rest.is_some() {
                    format!("`...rest` only works on growable arrays, not `{inner}`")
                } else {
                    format!("`{inner}` has {len} elements, but the pattern has {n}")
                };
                self.diags.push(
                    Diagnostic::error(codes::WRONG_LENGTH, msg)
                        .primary(p.span, "wrong number of elements"),
                );
                fail(self);
            }
            other => {
                if !other.is_error() {
                    self.pattern_mismatch(p.span, ty, "an array");
                }
                fail(self);
            }
        }
    }

    /// Types a pattern binding. In an or-pattern every alternative binds the same local, so
    /// they must agree.
    fn bind_local(&mut self, l: LocalId, t: &Ty, span: Span) {
        let f = self.stack.last().unwrap().id;
        let cur = &self.types.locals[f.0 as usize][l.0 as usize];
        if !cur.is_error() && cur != t && !t.is_error() {
            let prev = cur.clone();
            self.diags.push(
                Diagnostic::error(
                    codes::MISMATCHED_TYPES,
                    "a binding has different types in different alternatives",
                )
                .primary(
                    span,
                    format!("`{t}` here, `{prev}` in an earlier alternative"),
                ),
            );
            return;
        }
        self.set_var_ty(Var::Local(l), t.clone());
    }

    fn field_patterns(
        &mut self,
        p: &Pattern,
        fps: &[FieldPat],
        fields: &[(String, Ty)],
        what: &str,
    ) {
        let mut idx = Vec::new();
        for fp in fps {
            match fields.iter().position(|(n, _)| *n == fp.name) {
                Some(i) => {
                    idx.push(i as u32);
                    self.check_pattern(&fp.pat, &fields[i].1);
                }
                None => {
                    idx.push(u32::MAX);
                    self.diags.push(
                        Diagnostic::error(
                            codes::UNKNOWN_MEMBER,
                            format!("{what} has no field `{}`", fp.name),
                        )
                        .primary(fp.name_span, "unknown field"),
                    );
                    self.check_pattern(&fp.pat, &Ty::Error);
                }
            }
        }
        self.types.pat_fields.insert(p.id, idx);
    }

    /// Types the bindings of sub-patterns of a pattern that failed to check.
    fn wild_subpatterns(&mut self, args: &VariantPatArgs) {
        match args {
            VariantPatArgs::None => {}
            VariantPatArgs::Positional(ps) => {
                for sub in ps {
                    self.check_pattern(sub, &Ty::Error);
                }
            }
            VariantPatArgs::Named(fps) => {
                for f in fps {
                    self.check_pattern(&f.pat, &Ty::Error);
                }
            }
        }
    }

    /// A checked pattern, reduced for the exhaustiveness checker: one entry per alternative
    /// of an or-pattern (expanded through nested patterns, capped).
    fn to_pat(&self, p: &Pattern, ty: &Ty) -> Vec<Pat> {
        const CAP: usize = 256;
        let inner = match ty {
            Ty::Nullable(t) if p.looks_inside(ty) => (**t).clone(),
            _ => ty.clone(),
        };
        let leaf = |c: Ctor| vec![Pat::Ctor(c, Vec::new())];
        // All combinations of the sub-patterns' alternatives.
        let product = |cols: Vec<Vec<Pat>>, c: Ctor| -> Vec<Pat> {
            let mut acc: Vec<Vec<Pat>> = vec![Vec::new()];
            for col in cols {
                let mut next = Vec::new();
                for prefix in &acc {
                    for alt in &col {
                        if next.len() >= CAP {
                            break;
                        }
                        let mut v = prefix.clone();
                        v.push(alt.clone());
                        next.push(v);
                    }
                }
                acc = next;
            }
            acc.into_iter()
                .map(|args| Pat::Ctor(c.clone(), args))
                .collect()
        };
        let struct_fields = |sid: StructId| -> Vec<Ty> {
            self.program
                .struct_def(sid)
                .fields
                .iter()
                .map(|f| f.ty.clone())
                .collect()
        };
        let pats = match &p.kind {
            PatKind::Wild | PatKind::Bind(_) => return vec![Pat::Wild],
            PatKind::Or(alts) => {
                return alts
                    .iter()
                    .flat_map(|a| self.to_pat(a, ty))
                    .take(CAP)
                    .collect();
            }
            PatKind::As(_, sub) => return self.to_pat(sub, ty),
            PatKind::Lit(l) => match l {
                LitPat::Null => leaf(Ctor::Null),
                LitPat::Bool(b) => leaf(Ctor::Bool(*b)),
                LitPat::Int(v) if inner.is_int() => leaf(Ctor::Int(*v)),
                LitPat::Int(v) => leaf(Ctor::Float((*v as f64).to_bits())),
                LitPat::Float(f) => leaf(Ctor::Float(f.to_bits())),
                LitPat::Str(s) => leaf(Ctor::Str(s.clone())),
            },
            PatKind::Range(lo, hi) => leaf(Ctor::Range(format!("{lo:?}..{hi:?}"))),
            PatKind::Variant { args, .. } => {
                let Some(&(eid, vi)) = self.types.variants.get(&p.id) else {
                    return vec![Pat::Wild];
                };
                let tys: Vec<Ty> = self.program.enum_def(eid).variants[vi as usize]
                    .fields
                    .iter()
                    .map(|f| f.ty.clone())
                    .collect();
                let mut cols: Vec<Vec<Pat>> = vec![vec![Pat::Wild]; tys.len()];
                match args {
                    VariantPatArgs::None => {}
                    VariantPatArgs::Positional(ps) => {
                        for (i, (sub, t)) in ps.iter().zip(&tys).enumerate() {
                            cols[i] = self.to_pat(sub, t);
                        }
                    }
                    VariantPatArgs::Named(fps) => self.named_subpats(p, fps, &tys, &mut cols),
                }
                product(cols, Ctor::Variant(vi))
            }
            PatKind::Struct { sid, fields: fps } => {
                let tys = struct_fields(*sid);
                let mut cols = vec![vec![Pat::Wild]; tys.len()];
                self.named_subpats(p, fps, &tys, &mut cols);
                product(cols, Ctor::Single)
            }
            PatKind::Type {
                ty: Ty::Struct(sid, _),
                fields: Some(fps),
                ..
            } => {
                let tys = struct_fields(*sid);
                let mut cols = vec![vec![Pat::Wild]; tys.len()];
                self.named_subpats(p, fps, &tys, &mut cols);
                product(cols, Ctor::Single)
            }
            PatKind::Type { .. } => vec![Pat::Wild],
            PatKind::Slice {
                prefix,
                rest,
                suffix,
            } => {
                let n = prefix.len() + suffix.len();
                let elem_tys: Vec<Ty> = match &inner {
                    Ty::Tuple(ts) if ts.len() == n && rest.is_none() => ts.clone(),
                    Ty::Fixed(t, len) if *len as usize == n && rest.is_none() => {
                        vec![(**t).clone(); n]
                    }
                    Ty::Array(t) => vec![(**t).clone(); prefix.len() + suffix.len()],
                    _ => return vec![Pat::Wild],
                };
                let cols: Vec<Vec<Pat>> = prefix
                    .iter()
                    .chain(suffix)
                    .zip(&elem_tys)
                    .map(|(sub, t)| self.to_pat(sub, t))
                    .collect();
                let c = match &inner {
                    Ty::Array(_) => Ctor::Slice {
                        prefix: prefix.len(),
                        suffix: suffix.len(),
                        rest: rest.is_some(),
                    },
                    _ => Ctor::Single,
                };
                product(cols, c)
            }
        };
        if p.looks_inside(ty) {
            pats.into_iter()
                .map(|x| Pat::Ctor(Ctor::NonNull, vec![x]))
                .collect()
        } else {
            pats
        }
    }

    fn named_subpats(&self, p: &Pattern, fps: &[FieldPat], tys: &[Ty], cols: &mut [Vec<Pat>]) {
        let Some(idx) = self.types.pat_fields.get(&p.id) else {
            return;
        };
        for (fp, i) in fps.iter().zip(idx) {
            if let Some(t) = tys.get(*i as usize) {
                cols[*i as usize] = self.to_pat(&fp.pat, t);
            }
        }
    }

    /// `a ?? b`: `b` is used when `a` is null.
    fn nullish(&mut self, l: &Expr, r: &Expr, want: Option<&Expect>) -> Ty {
        let lt = match want {
            Some(w) if !matches!(w.ty, Ty::Void | Ty::Nullable(_)) => {
                self.infer_with(l, Some(&Expect::new(w.ty.clone().nullable())))
            }
            _ => self.infer(l),
        };
        match lt {
            Ty::Nullable(inner) => {
                // `a ?? b` with a nullable `b` stays nullable.
                let rt = self.infer_with(r, Some(&Expect::new((*inner).clone())));
                if rt == *inner || rt == Ty::Never || rt.is_error() {
                    return *inner;
                }
                if rt.widens_to(&inner) {
                    self.types.coercions.insert(r.id, (*inner).clone());
                    return *inner;
                }
                let n = Ty::Nullable(inner);
                if rt.widens_to(&n) {
                    if rt != n {
                        self.types.coercions.insert(r.id, n.clone());
                    }
                    return n;
                }
                let want = match &n {
                    Ty::Nullable(t) => (**t).clone(),
                    t => t.clone(),
                };
                self.mismatch(r.span, &Expect::new(want), &rt);
                Ty::Error
            }
            Ty::Error => {
                self.infer(r);
                Ty::Error
            }
            Ty::Null => self.infer(r),
            t => {
                // Not nullable: `b` is never used, but it still has to fit.
                self.check(r, &Expect::new(t.clone()));
                t
            }
        }
    }

    /// `t[i]` on a tuple: `i` must be an integer literal below the tuple's length.
    fn tuple_index(&mut self, ts: &[Ty], index: &Expr) -> Ty {
        match index.kind {
            ExprKind::Int(v, _) if (v as usize) < ts.len() => {
                self.set(index, Ty::INT);
                ts[v as usize].clone()
            }
            ExprKind::Int(v, _) => {
                self.set(index, Ty::INT);
                self.diags.push(
                    Diagnostic::error(
                        codes::TUPLE_INDEX,
                        format!("tuple index {v} is out of range"),
                    )
                    .primary(
                        index.span,
                        format!(
                            "the tuple has {} element{}",
                            ts.len(),
                            if ts.len() == 1 { "" } else { "s" }
                        ),
                    ),
                );
                Ty::Error
            }
            _ => {
                self.infer(index);
                self.diags.push(
                    Diagnostic::error(codes::TUPLE_INDEX, "tuple index must be a constant")
                        .primary(index.span, "not an integer literal")
                        .note("each tuple element has its own type, so the index must be known")
                        .help("use an integer literal, `t[0]`, or destructure: `const [a, b] = t`"),
                );
                Ty::Error
            }
        }
    }

    fn wrong_length(&mut self, e: &Expr, want: &Expect, expected: usize, found: usize) {
        let mut d = Diagnostic::error(
            codes::WRONG_LENGTH,
            format!(
                "expected {expected} element{}, found {found}",
                if expected == 1 { "" } else { "s" }
            ),
        )
        .primary(e.span, format!("this literal has {found}"));
        if let Some(o) = want.origin {
            d = d.secondary(o, format!("`{}` expected because of this", want.ty));
        }
        self.diags.push(d);
    }

    /// `[a, b, c]`: a tuple, fixed array or growable array as the expected type says;
    /// without one, a growable array whose element type comes from the elements.
    fn array_lit(&mut self, e: &Expr, elems: &[Expr], want: Option<&Expect>) -> Ty {
        let want = want.map(|w| match &w.ty {
            Ty::Nullable(inner) => Expect::from((**inner).clone(), w.origin),
            _ => w.clone(),
        });
        match want.as_ref().map(|w| (&w.ty, w)) {
            Some((Ty::Tuple(ts), w)) => {
                if ts.len() != elems.len() {
                    self.wrong_length(e, w, ts.len(), elems.len());
                    for x in elems {
                        self.infer(x);
                    }
                    return Ty::Error;
                }
                for (x, t) in elems.iter().zip(ts) {
                    self.check(x, &Expect::new(t.clone()));
                }
                w.ty.clone()
            }
            Some((Ty::Fixed(t, n), w)) => {
                if *n as usize != elems.len() {
                    self.wrong_length(e, w, *n as usize, elems.len());
                }
                for x in elems {
                    self.check(x, &Expect::new((**t).clone()));
                }
                w.ty.clone()
            }
            Some((Ty::Array(t), w)) => {
                for x in elems {
                    self.check(x, &Expect::new((**t).clone()));
                }
                w.ty.clone()
            }
            _ => {
                if elems.is_empty() {
                    self.diags.push(
                        Diagnostic::error(
                            codes::UNKNOWN_ELEMENT_TYPE,
                            "can't infer the element type of an empty array",
                        )
                        .primary(e.span, "no elements and no expected type")
                        .help("annotate the binding: `const xs: i32[] = []`"),
                    );
                    return Ty::Error;
                }
                // The first element that isn't an untyped literal decides the type, so
                // `[1, x]` takes `x`'s type; all literals: `int`, or `number` with a float.
                let elem = match elems.iter().find(|x| !untyped_lit(x)) {
                    Some(x) => self.infer(x),
                    None if elems.iter().any(is_float_lit) => Ty::NUMBER,
                    None => Ty::INT,
                };
                if elem.is_error() {
                    for x in elems {
                        self.infer(x);
                    }
                    return Ty::Error;
                }
                let want = Expect::new(elem.clone());
                for x in elems {
                    self.check(x, &want);
                }
                Ty::Array(Box::new(elem))
            }
        }
    }

    /// `[value; count]`: a fixed array.
    fn repeat_lit(&mut self, e: &Expr, value: &Expr, count: u64, want: Option<&Expect>) -> Ty {
        match want.map(|w| (&w.ty, w)) {
            Some((Ty::Fixed(t, n), w)) => {
                if *n != count {
                    self.wrong_length(e, w, *n as usize, count as usize);
                }
                self.check(value, &Expect::new((**t).clone()));
                w.ty.clone()
            }
            _ => {
                let t = self.infer(value);
                if t.is_error() {
                    return Ty::Error;
                }
                Ty::Fixed(Box::new(t), count)
            }
        }
    }

    /// `start..end`: both bounds join to one integer type.
    fn range(&mut self, e: &Expr, start: &Expr, end: &Expr, want: Option<&Expect>) -> Ty {
        let hint = want.and_then(|w| match &w.ty {
            Ty::Range(it) => Some(Expect::new(Ty::Int(*it))),
            _ => None,
        });
        let (lt, rt) = self.operands(start, end, hint.as_ref());
        let t = if lt == rt || rt.is_error() {
            lt
        } else if lt.is_error() {
            rt
        } else if rt.widens_to(&lt) {
            self.types.coercions.insert(end.id, lt.clone());
            lt
        } else if lt.widens_to(&rt) {
            self.types.coercions.insert(start.id, rt.clone());
            rt
        } else {
            let mut d = Diagnostic::error(
                codes::MISMATCHED_TYPES,
                "the bounds of a range have different types",
            )
            .secondary(start.span, format!("`{lt}`"))
            .secondary(end.span, format!("`{rt}`"))
            .help("convert one side explicitly with `as`");
            d.labels[1].primary = true;
            self.diags.push(d);
            return Ty::Error;
        };
        match t {
            Ty::Int(it) => Ty::Range(it),
            Ty::Error => Ty::Error,
            other => {
                self.invalid_operand(e.span, "`..`", &other, "integer bounds");
                Ty::Error
            }
        }
    }

    /// The type of an assignment target, and the span that declared it.
    fn target_ty(&mut self, t: &Target) -> (Ty, Option<Span>) {
        match t {
            Target::Var(v) => {
                let span = self.var_binding(*v).span;
                (self.var_ty(*v, span), Some(span))
            }
            Target::Field {
                id,
                object,
                name,
                name_span,
            } => {
                let ot = self.infer(object);
                match ot {
                    Ty::Struct(sid, _) => {
                        let t = self.field(*id, sid, name, *name_span);
                        let span = self
                            .program
                            .struct_def(sid)
                            .field(name)
                            .map(|(_, f)| f.span);
                        (t, span)
                    }
                    Ty::Error => (Ty::Error, None),
                    other => {
                        self.diags.push(
                            Diagnostic::error(
                                codes::UNKNOWN_MEMBER,
                                format!("`{other}` has no field `{name}`"),
                            )
                            .primary(*name_span, "unknown field"),
                        );
                        (Ty::Error, None)
                    }
                }
            }
            Target::Index { object, index } => {
                let ot = self.infer(object);
                match ot {
                    Ty::Array(t) | Ty::Fixed(t, _) => {
                        self.check(index, &Expect::new(Ty::INT));
                        (*t, None)
                    }
                    Ty::Tuple(ts) => (self.tuple_index(&ts, index), None),
                    Ty::Error => {
                        self.infer(index);
                        (Ty::Error, None)
                    }
                    Ty::Str => {
                        self.infer(index);
                        self.diags.push(
                            Diagnostic::error(codes::INVALID_OPERANDS, "strings are immutable")
                                .primary(object.span, "can't assign to a character of a string")
                                .help("build a new string instead, e.g. with a template"),
                        );
                        (Ty::Error, None)
                    }
                    other => {
                        self.infer(index);
                        self.diags.push(
                            Diagnostic::error(
                                codes::INVALID_OPERANDS,
                                format!("`{other}` can't be indexed"),
                            )
                            .primary(object.span, format!("has type `{other}`")),
                        );
                        (Ty::Error, None)
                    }
                }
            }
        }
    }

    /// Looks up a struct field for `object.name` and records its index under `id`.
    fn field(&mut self, id: ExprId, sid: StructId, name: &str, span: Span) -> Ty {
        let def = self.program.struct_def(sid);
        match def.field(name) {
            Some((i, f)) => {
                self.types.fields.insert(id, i as u32);
                f.ty.clone()
            }
            None => {
                let mut d = Diagnostic::error(
                    codes::UNKNOWN_MEMBER,
                    format!("`{}` has no field `{name}`", def.name),
                )
                .primary(span, "unknown field")
                .secondary(def.name_span, format!("`{}` is declared here", def.name));
                if def.method(name).is_some() {
                    d = d.help(format!("`{name}` is a method; call it: `{name}()`"));
                } else if !def.fields.is_empty() {
                    let names: Vec<&str> = def.fields.iter().map(|f| f.name.as_str()).collect();
                    d = d.note(format!("its fields are: {}", names.join(", ")));
                }
                self.diags.push(d);
                Ty::Error
            }
        }
    }

    /// A struct literal: every field exactly once.
    fn object(
        &mut self,
        e: &Expr,
        ty: Option<StructId>,
        fields: &[FieldInit],
        want: Option<&Expect>,
    ) -> Ty {
        let sid = match (ty, want.map(|w| &w.ty)) {
            (Some(s), _) => s,
            (None, Some(Ty::Struct(s, _))) => *s,
            _ => {
                for f in fields {
                    self.infer(&f.value);
                }
                self.diags.push(
                    Diagnostic::error(codes::UNKNOWN_OBJECT_TYPE, "can't tell which struct this object literal builds")
                        .primary(e.span, "no struct type is expected here")
                        .help("name the type: `Point { x: 1, y: 2 }`, or annotate the binding: `const p: Point = { … }`"),
                );
                return Ty::Error;
            }
        };
        let def = self.program.struct_def(sid);
        let mut seen: Vec<Option<Span>> = vec![None; def.fields.len()];
        let mut order = Vec::new();
        for f in fields {
            match def.field(&f.name) {
                Some((i, fd)) => {
                    if let Some(prev) = seen[i] {
                        self.diags.push(
                            Diagnostic::error(
                                codes::DUPLICATE_FIELD,
                                format!("field `{}` is given twice", f.name),
                            )
                            .primary(f.name_span, "given again here")
                            .secondary(prev, "first given here"),
                        );
                    }
                    seen[i] = Some(f.name_span);
                    order.push(i as u32);
                    self.check(&f.value, &Expect::from(fd.ty.clone(), Some(fd.span)));
                }
                None => {
                    self.infer(&f.value);
                    self.diags.push(
                        Diagnostic::error(
                            codes::UNKNOWN_MEMBER,
                            format!("`{}` has no field `{}`", def.name, f.name),
                        )
                        .primary(f.name_span, "unknown field")
                        .secondary(def.name_span, format!("`{}` is declared here", def.name)),
                    );
                    order.push(u32::MAX);
                }
            }
        }
        let missing: Vec<&str> = def
            .fields
            .iter()
            .zip(&seen)
            .filter(|(_, s)| s.is_none())
            .map(|(f, _)| f.name.as_str())
            .collect();
        if !missing.is_empty() {
            let list = missing
                .iter()
                .map(|m| format!("`{m}`"))
                .collect::<Vec<_>>()
                .join(", ");
            self.diags.push(
                Diagnostic::error(
                    codes::MISSING_FIELDS,
                    format!(
                        "missing field{} {list}",
                        if missing.len() == 1 { "" } else { "s" }
                    ),
                )
                .primary(e.span, format!("`{}` needs every field", def.name))
                .secondary(def.name_span, format!("`{}` is declared here", def.name)),
            );
        }
        self.types.objects.insert(e.id, (sid, order));
        self.program.struct_ty(sid)
    }

    /// Whether `==` is defined: primitives, strings, and structs whose fields all are.
    fn equatable(&self, t: &Ty) -> bool {
        match t {
            Ty::Int(_) | Ty::Float(_) | Ty::Bool | Ty::Char | Ty::Str | Ty::Error => true,
            Ty::Struct(sid, _) => self
                .program
                .struct_def(*sid)
                .fields
                .iter()
                .all(|f| self.equatable(&f.ty)),
            Ty::Tuple(ts) => ts.iter().all(|t| self.equatable(t)),
            Ty::Nullable(t) => self.equatable(t),
            Ty::Enum(id, _) => self
                .program
                .enum_def(*id)
                .variants
                .iter()
                .all(|v| v.fields.iter().all(|f| self.equatable(&f.ty))),
            Ty::Fixed(t, _) => self.equatable(t),
            Ty::Range(_) => true,
            _ => false,
        }
    }

    /// Parses and checks a template part's format spec against the part's type.
    fn format_spec(&mut self, e: &Expr, spec: &str) {
        let t = self.types.final_ty(e.id).clone();
        if t.is_error() {
            return;
        }
        match crate::format::parse(spec).and_then(|f| crate::format::check(&f, &t).map(|()| f)) {
            Ok(f) => {
                self.types.formats.insert(e.id, f);
            }
            Err(why) => self.diags.push(
                Diagnostic::error(
                    codes::INVALID_FORMAT_SPEC,
                    format!("invalid format spec `{spec}`"),
                )
                .primary(e.span, format!("formatted with `:{spec}`"))
                .note(why),
            ),
        }
    }

    fn int_lit(
        &mut self,
        e: &Expr,
        v: i128,
        suffix: Option<NumSuffix>,
        want: Option<&Expect>,
    ) -> Ty {
        let t = match (suffix, want.map(|w| &w.ty)) {
            (Some(s), _) => suffix_ty(s),
            (None, Some(t @ (Ty::Int(_) | Ty::Float(_)))) => t.clone(),
            (None, Some(Ty::Nullable(inner))) if inner.is_numeric() => (**inner).clone(),
            _ => Ty::INT,
        };
        if let Ty::Int(it) = t
            && !it.contains(v)
        {
            self.lit_out_of_range(e.span, v, it);
            return Ty::Error;
        }
        t
    }

    fn lit_out_of_range(&mut self, span: Span, v: i128, it: IntTy) {
        self.diags.push(
            Diagnostic::error(
                codes::LITERAL_OUT_OF_RANGE,
                format!("literal out of range for `{}`", it.name()),
            )
            .primary(span, format!("`{v}` doesn't fit"))
            .note(format!(
                "`{}` holds values from {} to {}",
                it.name(),
                it.min(),
                it.max()
            )),
        );
    }

    fn invalid_operand(&mut self, span: Span, op: &str, t: &Ty, needs: &str) {
        if t.is_error() {
            return;
        }
        self.diags.push(
            Diagnostic::error(
                codes::INVALID_OPERANDS,
                format!("{op} can't be applied to `{t}`"),
            )
            .primary(span, format!("has type `{t}`"))
            .note(format!("{op} needs {needs}")),
        );
        if let Ty::Nullable(_) = t {
            let d = self.diags.pop().expect("just pushed");
            self.diags.push(d.help(NULL_HELP));
        }
    }

    fn unary(&mut self, e: &Expr, op: UnOp, inner: &Expr, want: Option<&Expect>) -> Ty {
        // `-128i8`, `-9223372036854775808`: the range check sees the negated value.
        if op == UnOp::Neg
            && let ExprKind::Int(v, suffix) = inner.kind
        {
            let t = match (suffix, want.map(|w| &w.ty)) {
                (Some(s), _) => suffix_ty(s),
                (None, Some(t @ (Ty::Int(_) | Ty::Float(_)))) => t.clone(),
                _ => Ty::INT,
            };
            self.set(inner, t.clone());
            return match t {
                Ty::Int(it) if !it.signed() => {
                    self.invalid_operand(e.span, "unary `-`", &t, "a signed integer or a float");
                    Ty::Error
                }
                Ty::Int(it) if !it.contains(-(v as i128)) => {
                    self.lit_out_of_range(e.span, -(v as i128), it);
                    Ty::Error
                }
                t => t,
            };
        }
        let t = match want {
            Some(w) if matches!(op, UnOp::Neg | UnOp::Plus | UnOp::BitNot) && w.ty.is_numeric() => {
                self.infer_with(inner, Some(w))
            }
            _ => self.infer(inner),
        };
        match op {
            UnOp::Neg => match &t {
                Ty::Int(it) if it.signed() => t,
                Ty::Float(_) | Ty::Error => t,
                _ => {
                    self.invalid_operand(e.span, "unary `-`", &t, "a signed integer or a float");
                    Ty::Error
                }
            },
            UnOp::Plus => {
                if t.is_numeric() || t.is_error() {
                    t
                } else {
                    self.invalid_operand(e.span, "unary `+`", &t, "a number");
                    Ty::Error
                }
            }
            UnOp::Not => {
                if matches!(t, Ty::Bool | Ty::Error) {
                    Ty::Bool
                } else {
                    self.invalid_operand(e.span, "`!`", &t, "a `bool`");
                    Ty::Error
                }
            }
            UnOp::BitNot => {
                if t.is_int() || t.is_error() {
                    t
                } else {
                    self.invalid_operand(e.span, "`~`", &t, "an integer");
                    Ty::Error
                }
            }
            _ => unreachable!("handled by the resolver"),
        }
    }

    /// Types both operands of a binary operator and joins them to one type.
    fn operands(&mut self, l: &Expr, r: &Expr, hint: Option<&Expect>) -> (Ty, Ty) {
        if untyped_lit(l) && !untyped_lit(r) {
            let rt = self.infer_with(r, hint);
            let lt = self.check_or_join(l, &rt);
            return (lt, rt);
        }
        let lt = self.infer_with(l, hint);
        let rt = self.infer_with(r, Some(&Expect::new(lt.clone())));
        (lt, rt)
    }

    fn check_or_join(&mut self, e: &Expr, to: &Ty) -> Ty {
        let t = self.infer_with(e, Some(&Expect::new(to.clone())));
        if t.widens_to(to) && t != *to {
            self.types.coercions.insert(e.id, to.clone());
            return to.clone();
        }
        t
    }

    /// The common type of two operands, inserting a widening on one side if needed.
    fn join(&mut self, op_span: Span, op: BinOp, l: &Expr, lt: &Ty, r: &Expr, rt: &Ty) -> Ty {
        if lt == rt || rt.is_error() {
            return lt.clone();
        }
        if lt.is_error() {
            return rt.clone();
        }
        if rt.widens_to(lt) {
            self.types.coercions.insert(r.id, lt.clone());
            return lt.clone();
        }
        if lt.widens_to(rt) {
            self.types.coercions.insert(l.id, rt.clone());
            return rt.clone();
        }
        let mut d = Diagnostic::error(
            codes::INVALID_OPERANDS,
            format!("`{}` can't be applied to `{lt}` and `{rt}`", op.as_str()),
        )
        .secondary(l.span, format!("`{lt}`"))
        .secondary(r.span, format!("`{rt}`"));
        d.labels[1].primary = true;
        if lt.is_numeric() && rt.is_numeric() {
            d = d.help("convert one side explicitly with `as`");
        }
        let _ = op_span;
        self.diags.push(d);
        Ty::Error
    }

    fn binary(&mut self, e: &Expr, op: BinOp, l: &Expr, r: &Expr, want: Option<&Expect>) -> Ty {
        use BinOp::*;
        match op {
            And | Or => {
                self.check(l, &Expect::new(Ty::Bool));
                let f = self.facts(l);
                let known = if op == And { f.when_true } else { f.when_false };
                self.with_facts(known, |c| c.check(r, &Expect::new(Ty::Bool)));
                Ty::Bool
            }
            Nullish => self.nullish(l, r, want),
            Shl | Shr | UShr => {
                let hint = want.filter(|w| w.ty.is_int());
                let lt = self.infer_with(l, hint);
                let rt = self.infer_with(r, Some(&Expect::new(Ty::Int(IntTy::U32))));
                if !lt.is_int() && !lt.is_error() {
                    self.invalid_operand(l.span, &format!("`{}`", op.as_str()), &lt, "an integer");
                    return Ty::Error;
                }
                if !rt.is_int() && !rt.is_error() {
                    self.invalid_operand(r.span, "a shift amount", &rt, "an integer");
                }
                lt
            }
            Add | Sub | Mul | Div | Rem | Pow | BitAnd | BitOr | BitXor => {
                let hint = want.filter(|w| w.ty.is_numeric());
                let (lt, rt) = self.operands(l, r, hint);
                let t = self.join(e.span, op, l, &lt, r, &rt);
                let ok = match op {
                    Add => t.is_numeric() || t == Ty::Str,
                    BitAnd | BitOr | BitXor => t.is_int(),
                    _ => t.is_numeric(),
                };
                if !ok && !t.is_error() {
                    let needs = match op {
                        Add => "numbers or strings",
                        BitAnd | BitOr | BitXor => "integers",
                        _ => "numbers",
                    };
                    self.invalid_operand(e.span, &format!("`{}`", op.as_str()), &t, needs);
                    return Ty::Error;
                }
                t
            }
            Lt | Le | Gt | Ge | Eq | Ne => {
                let (lt, rt) = self.operands(l, r, None);
                let t = self.join(e.span, op, l, &lt, r, &rt);
                let ok = match op {
                    Eq | Ne => self.equatable(&t),
                    _ => matches!(
                        t,
                        Ty::Int(_) | Ty::Float(_) | Ty::Char | Ty::Str | Ty::Error
                    ),
                };
                if !ok {
                    self.invalid_operand(
                        e.span,
                        &format!("`{}`", op.as_str()),
                        &t,
                        "comparable values",
                    );
                }
                Ty::Bool
            }
        }
    }

    /// `x op= value`
    fn compound(&mut self, e: &Expr, op: BinOp, vt: Ty, value: &Expr) {
        use BinOp::*;
        match op {
            And | Or => {
                if !matches!(vt, Ty::Bool | Ty::Error) {
                    self.invalid_operand(e.span, &format!("`{}=`", op.as_str()), &vt, "a `bool`");
                }
                self.check(value, &Expect::new(Ty::Bool));
            }
            Shl | Shr | UShr => {
                if !vt.is_int() && !vt.is_error() {
                    self.invalid_operand(e.span, &format!("`{}=`", op.as_str()), &vt, "an integer");
                }
                let rt = self.infer_with(value, Some(&Expect::new(Ty::Int(IntTy::U32))));
                if !rt.is_int() && !rt.is_error() {
                    self.invalid_operand(value.span, "a shift amount", &rt, "an integer");
                }
            }
            Nullish => {
                if !matches!(vt, Ty::Nullable(_) | Ty::Error) {
                    self.invalid_operand(e.span, "`??=`", &vt, "a nullable (`T?`) target");
                }
                self.check(value, &Expect::new(vt.clone()));
            }
            _ => {
                self.check(value, &Expect::new(vt.clone()));
                let ok = match op {
                    Add => vt.is_numeric() || vt == Ty::Str,
                    BitAnd | BitOr | BitXor => vt.is_int(),
                    _ => vt.is_numeric(),
                };
                if !ok {
                    self.invalid_operand(e.span, &format!("`{}=`", op.as_str()), &vt, "numbers");
                }
            }
        }
    }

    fn if_expr(
        &mut self,
        cond: &Expr,
        then: &Expr,
        else_: Option<&Expr>,
        want: Option<&Expect>,
    ) -> Ty {
        self.check(cond, &Expect::new(Ty::Bool));
        let facts = self.facts(cond);
        let (ft, ff) = (facts.when_true, facts.when_false);
        match else_ {
            Some(els) => match want {
                Some(w) if !matches!(w.ty, Ty::Void) => {
                    self.with_facts(ft, |c| c.check(then, w));
                    self.with_facts(ff, |c| c.check(els, w));
                    w.ty.clone()
                }
                _ => {
                    let tt = self.with_facts(ft, |c| c.infer(then));
                    let et = self.with_facts(ff, |c| c.infer(els));
                    if want.is_some_and(|w| w.ty == Ty::Void) {
                        return Ty::Void;
                    }
                    self.unify_branches(then, &tt, els, &et)
                }
            },
            None => {
                let inner = match want.map(|w| &w.ty) {
                    Some(Ty::Nullable(t)) => Some(Expect::new((**t).clone())),
                    _ => None,
                };
                let tt = self.with_facts(ft, |c| match &inner {
                    Some(w) => c.check(then, w),
                    None => c.infer(then),
                });
                match tt {
                    Ty::Void | Ty::Never => Ty::Void,
                    t => t.nullable(),
                }
            }
        }
    }

    fn unify_branches(&mut self, then: &Expr, tt: &Ty, els: &Expr, et: &Ty) -> Ty {
        match (tt, et) {
            (Ty::Never, t) | (t, Ty::Never) => t.clone(),
            (a, b) if a == b => a.clone(),
            // `if (c) null else x`: `x`'s type made nullable.
            (Ty::Null, t) | (t, Ty::Null) if !matches!(t, Ty::Void | Ty::Error) => {
                let n = t.clone().nullable();
                for (e, ty) in [(then, tt), (els, et)] {
                    if *ty != n {
                        self.types.coercions.insert(e.id, n.clone());
                    }
                }
                n
            }
            (a, b) if b.widens_to(a) => {
                self.types.coercions.insert(els.id, a.clone());
                a.clone()
            }
            (a, b) if a.widens_to(b) => {
                self.types.coercions.insert(then.id, b.clone());
                b.clone()
            }
            (a, b) if a.is_error() || b.is_error() => Ty::Error,
            (a, b) => {
                let mut d = Diagnostic::error(
                    codes::MISMATCHED_TYPES,
                    "`if` and `else` have different types",
                )
                .secondary(then.span, format!("`{a}`"))
                .secondary(els.span, format!("`{b}`"));
                d.labels[1].primary = true;
                self.diags.push(d);
                Ty::Error
            }
        }
    }

    fn ret(&mut self, e: &Expr, value: Option<&Expr>) {
        if self.func().is_entry {
            if let Some(v) = value {
                self.infer(v);
            }
            return;
        }
        let declared = match &self.ctx().ret {
            Ret::Declared(t, origin) => Some((t.clone(), *origin)),
            Ret::Infer(_) => None,
        };
        match (declared, value) {
            (Some((t, origin)), Some(v)) => {
                if t == Ty::Void {
                    self.infer(v);
                } else {
                    self.check(v, &Expect::from(t, origin));
                }
            }
            (Some((t, origin)), None) => {
                if t != Ty::Void && !t.is_error() {
                    let mut d =
                        Diagnostic::error(codes::MISMATCHED_TYPES, "`return` without a value")
                            .primary(e.span, format!("expected a `{t}` value"));
                    if let Some(o) = origin {
                        d = d.secondary(o, "the return type is declared here");
                    }
                    self.diags.push(d);
                }
            }
            (None, v) => {
                let t = match v {
                    Some(v) => self.infer(v),
                    None => Ty::Void,
                };
                if let Ret::Infer(rets) = &mut self.ctx().ret {
                    rets.push((t, v.map_or(e.span, |v| v.span)));
                }
            }
        }
    }

    /// Checks a call of the function `id` (a free function or a method; `this` isn't an
    /// argument) and returns its result type.
    fn call_fn(&mut self, e: &Expr, id: FnId, args: &[Expr]) -> Ty {
        let f = self.program.func(id);
        if f.ret.is_none() {
            match self.state[id.0 as usize] {
                FnState::InProgress => {
                    self.diags.push(
                        Diagnostic::error(
                            codes::RECURSIVE_NEEDS_RETURN_TYPE,
                            format!("recursive function `{}` needs a return type", f.name),
                        )
                        .primary(f.name_span, "add a return type, e.g. `: i64`")
                        .secondary(e.span, "recursive call here")
                        .note("the return type of a recursive function can't be inferred"),
                    );
                    self.types.sigs[id.0 as usize].ret = Ty::Error;
                    self.state[id.0 as usize] = FnState::Done;
                }
                FnState::Unchecked => self.check_fn(id),
                FnState::Done => {}
            }
        }
        let sig = self.types.sigs[id.0 as usize].clone();
        if args.len() != sig.params.len() {
            self.diags.push(
                Diagnostic::error(
                    codes::WRONG_ARG_COUNT,
                    format!(
                        "`{}` takes {} argument{}, but {} were given",
                        f.name,
                        sig.params.len(),
                        if sig.params.len() == 1 { "" } else { "s" },
                        args.len()
                    ),
                )
                .primary(e.span, "wrong number of arguments")
                .secondary(f.name_span, "declared here"),
            );
            for a in args {
                self.infer(a);
            }
            return sig.ret;
        }
        for (a, (p, pid)) in args.iter().zip(sig.params.iter().zip(&f.params)) {
            let origin = Some(f.local(*pid).span);
            self.check(a, &Expect::from(p.clone(), origin));
        }
        sig.ret
    }

    fn call(&mut self, e: &Expr, callee: &Expr, args: &[Expr]) -> Ty {
        match &callee.kind {
            ExprKind::Fn(id) => {
                self.set(callee, Ty::Void);
                self.call_fn(e, *id, args)
            }
            ExprKind::Builtin(b) => {
                self.set(callee, Ty::Void);
                self.builtin(e, *b, args)
            }
            ExprKind::Member {
                object,
                name,
                name_span,
            } => {
                let t = self.infer(object);
                self.set(callee, Ty::Void);
                if let Some(r) = self.builtin_method(e, &t, name, *name_span, args) {
                    return r;
                }
                if let Ty::Struct(sid, sname) = &t {
                    let def = self.program.struct_def(*sid);
                    if let Some(m) = def.method(name) {
                        self.types.methods.insert(e.id, m);
                        return self.call_fn(e, m, args);
                    }
                    let what = if def.field(name).is_some() {
                        "is a field, not a method"
                    } else {
                        "isn't declared"
                    };
                    self.diags.push(
                        Diagnostic::error(
                            codes::UNKNOWN_MEMBER,
                            format!("`{sname}` has no method `{name}`"),
                        )
                        .primary(*name_span, format!("`{name}` {what}")),
                    );
                    for a in args {
                        self.infer(a);
                    }
                    return Ty::Error;
                }
                // `x.toString()` on primitives.
                if name == "toString" && is_printable(&t) {
                    if !args.is_empty() {
                        self.diags.push(
                            Diagnostic::error(
                                codes::WRONG_ARG_COUNT,
                                "`toString` takes no arguments",
                            )
                            .primary(e.span, "wrong number of arguments"),
                        );
                    }
                    return Ty::Str;
                }
                for a in args {
                    self.infer(a);
                }
                if !t.is_error() {
                    self.diags.push(
                        Diagnostic::error(
                            codes::UNKNOWN_MEMBER,
                            format!("`{t}` has no method `{name}`"),
                        )
                        .primary(*name_span, "unknown member"),
                    );
                }
                Ty::Error
            }
            _ => {
                let t = self.infer(callee);
                for a in args {
                    self.infer(a);
                }
                if !t.is_error() {
                    self.diags.push(
                        Diagnostic::error(codes::NOT_CALLABLE, format!("`{t}` isn't a function"))
                            .primary(callee.span, "called here"),
                    );
                }
                Ty::Error
            }
        }
    }

    /// Methods of built-in types (`xs.push(v)`, `r.step(n)`); `None` if `name` isn't one.
    fn builtin_method(
        &mut self,
        e: &Expr,
        t: &Ty,
        name: &str,
        name_span: Span,
        args: &[Expr],
    ) -> Option<Ty> {
        let _ = name_span;
        let (arity_ok, expected, ret) = match (t, name) {
            (Ty::Array(elem), "push") => {
                let want = Expect::new((**elem).clone());
                for a in args {
                    self.check(a, &want);
                }
                (!args.is_empty(), "at least 1 argument", Ty::Void)
            }
            (Ty::Array(elem), "pop") => {
                for a in args {
                    self.infer(a);
                }
                (args.is_empty(), "no arguments", (**elem).clone().nullable())
            }
            (Ty::Range(it), "step") => {
                let want = Expect::new(Ty::Int(*it));
                for a in args {
                    self.check(a, &want);
                }
                (args.len() == 1, "1 argument", Ty::Range(*it))
            }
            _ => return None,
        };
        if !arity_ok {
            self.diags.push(
                Diagnostic::error(
                    codes::WRONG_ARG_COUNT,
                    format!("`{name}` takes {expected}, but {} were given", args.len()),
                )
                .primary(e.span, "wrong number of arguments"),
            );
        }
        Some(ret)
    }

    fn builtin(&mut self, e: &Expr, b: Builtin, args: &[Expr]) -> Ty {
        let arity = |n: &[usize]| n.contains(&args.len());
        let (ok, ret) = match b {
            Builtin::Print | Builtin::Println => {
                for a in args {
                    let t = self.infer(a);
                    if !is_printable(&t) {
                        self.diags.push(
                            Diagnostic::error(
                                codes::INVALID_OPERANDS,
                                format!("`{t}` can't be printed"),
                            )
                            .primary(a.span, "no string form"),
                        );
                    }
                }
                (
                    if b == Builtin::Print {
                        arity(&[1])
                    } else {
                        arity(&[0, 1])
                    },
                    Ty::Void,
                )
            }
            Builtin::Assert => {
                if let Some(c) = args.first() {
                    self.check(c, &Expect::new(Ty::Bool));
                }
                if let Some(m) = args.get(1) {
                    self.check(m, &Expect::new(Ty::Str));
                }
                (arity(&[1, 2]), Ty::Void)
            }
            Builtin::Panic => {
                if let Some(m) = args.first() {
                    self.check(m, &Expect::new(Ty::Str));
                }
                (arity(&[1]), Ty::Never)
            }
        };
        if !ok {
            let expected = match b {
                Builtin::Print | Builtin::Panic => "1 argument",
                Builtin::Println => "0 or 1 arguments",
                Builtin::Assert => "1 or 2 arguments",
            };
            self.diags.push(
                Diagnostic::error(
                    codes::WRONG_ARG_COUNT,
                    format!(
                        "`{}` takes {expected}, but {} were given",
                        b.name(),
                        args.len()
                    ),
                )
                .primary(e.span, "wrong number of arguments"),
            );
        }
        ret
    }
}
