//! Type checking (spec §2.1–§2.2, §3.1, §5.1) over the HIR.
//!
//! Bidirectional: [`Checker::check`] pushes an expected type down (so `const x: u8 = 1` types
//! the literal as `u8`), [`Checker::infer`] computes a type bottom-up. Implicit conversions
//! happen only when lossless; they're recorded in [`Types::coercions`] for lowering.
//!
//! Return types are inferred on demand: a call to a function whose return type is still
//! unknown checks that function first. Reaching a function that is already being checked
//! means it's recursive, and recursive functions must declare their return type.

use crate::hir::*;
use crate::ty::{FloatTy, IntTy, Ty};
use ppl_diag::{Diagnostic, Span, codes};
use ppl_syntax::ast::{BinOp, UnOp};
use ppl_syntax::token::NumSuffix;
use std::collections::HashMap;

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
        },
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
}

/// An integer literal without a suffix (possibly negated) adapts to its context.
fn untyped_lit(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::Int(_, None) | ExprKind::Float(_, None) => true,
        ExprKind::Unary(UnOp::Neg | UnOp::Plus, inner) => untyped_lit(inner),
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
    matches!(
        t,
        Ty::Int(_) | Ty::Float(_) | Ty::Bool | Ty::Char | Ty::Str | Ty::Error | Ty::Never
    )
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
            && **inner == want.ty
        {
            d = d.note("an `if` without `else` produces `null` when the condition is false");
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
        let mut diverges = false;
        for s in &b.stmts {
            match s {
                Stmt::Let { var, init, span } => self.let_stmt(*var, init.as_ref(), *span),
                Stmt::Expr(e) => {
                    if self.infer(e) == Ty::Never {
                        diverges = true;
                    }
                }
            }
        }
        match &b.tail {
            Some(t) => match want {
                Some(w) => self.check(t, w),
                None => self.infer(t),
            },
            None if diverges => Ty::Never,
            None => Ty::Void,
        }
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
            ExprKind::Var(v) => self.var_ty(*v, e.span),
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
            ExprKind::Assign { op, var, value } => {
                let vt = self.var_ty(*var, e.span);
                match op {
                    None => {
                        let b = self.var_binding(*var);
                        self.check(value, &Expect::from(vt, Some(b.span)));
                    }
                    Some(op) => {
                        self.compound(e, *op, vt, value);
                    }
                }
                Ty::Void
            }
            ExprKind::IncDec { var, .. } => {
                let t = self.var_ty(*var, e.span);
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
                    (Ty::Str, "length") => Ty::INT,
                    (Ty::Error, _) => Ty::Error,
                    _ => {
                        self.diags.push(
                            Diagnostic::error(
                                codes::UNKNOWN_MEMBER,
                                format!("`{ot}` has no member `{name}`"),
                            )
                            .primary(*name_span, "unknown member"),
                        );
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
                    Ty::Error => Ty::Error,
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
                    if !is_printable(&t) {
                        self.diags.push(
                            Diagnostic::error(
                                codes::INVALID_OPERANDS,
                                format!("`{t}` can't be put in a template"),
                            )
                            .primary(p.expr.span, "has no string form"),
                        );
                    }
                }
                Ty::Str
            }
            ExprKind::If { cond, then, else_ } => self.if_expr(cond, then, else_.as_deref(), want),
            ExprKind::Block(b) => self.block(b, want),
            ExprKind::Loop(b) => {
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
                self.check(cond, &Expect::new(Ty::Bool));
                self.ctx().loops.push(LoopCtx {
                    produces_value: false,
                    want: None,
                    breaks: Vec::new(),
                });
                self.infer(body);
                if let Some(u) = update {
                    self.infer(u);
                }
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
            ExprKind::Error => Ty::Error,
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
                self.check(r, &Expect::new(Ty::Bool));
                Ty::Bool
            }
            Nullish => {
                self.infer(l);
                self.infer(r);
                self.diags.push(
                    Diagnostic::error(codes::NOT_IMPLEMENTED, "`??` isn't implemented yet")
                        .primary(e.span, "not supported by this compiler version"),
                );
                Ty::Error
            }
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
                    Eq | Ne => matches!(
                        t,
                        Ty::Int(_) | Ty::Float(_) | Ty::Bool | Ty::Char | Ty::Str | Ty::Error
                    ),
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
                self.infer(value);
                self.diags.push(
                    Diagnostic::error(codes::NOT_IMPLEMENTED, "`??=` isn't implemented yet")
                        .primary(e.span, "not supported by this compiler version"),
                );
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
        match else_ {
            Some(els) => match want {
                Some(w) if !matches!(w.ty, Ty::Void) => {
                    self.check(then, w);
                    self.check(els, w);
                    w.ty.clone()
                }
                _ => {
                    let tt = self.infer(then);
                    let et = self.infer(els);
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
                let tt = match &inner {
                    Some(w) => self.check(then, w),
                    None => self.infer(then),
                };
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

    fn call(&mut self, e: &Expr, callee: &Expr, args: &[Expr]) -> Ty {
        match &callee.kind {
            ExprKind::Fn(id) => {
                self.set(callee, Ty::Void);
                let f = self.program.func(*id);
                if f.ret.is_none() {
                    match self.state[id.0 as usize] {
                        FnState::InProgress => {
                            self.diags.push(
                                Diagnostic::error(
                                    codes::RECURSIVE_NEEDS_RETURN_TYPE,
                                    format!("recursive function `{}` needs a return type", f.name),
                                )
                                .primary(f.name_span, "add a return type, e.g. `: i64`")
                                .secondary(callee.span, "recursive call here")
                                .note("the return type of a recursive function can't be inferred"),
                            );
                            self.types.sigs[id.0 as usize].ret = Ty::Error;
                            self.state[id.0 as usize] = FnState::Done;
                        }
                        FnState::Unchecked => self.check_fn(*id),
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
            ExprKind::Builtin(b) => {
                self.set(callee, Ty::Void);
                self.builtin(e, *b, args)
            }
            // `x.toString()` on primitives.
            ExprKind::Member {
                object,
                name,
                name_span,
            } if name == "toString" => {
                let t = self.infer(object);
                self.set(callee, Ty::Void);
                if !is_printable(&t) {
                    self.diags.push(
                        Diagnostic::error(
                            codes::UNKNOWN_MEMBER,
                            format!("`{t}` has no member `toString`"),
                        )
                        .primary(*name_span, "unknown member"),
                    );
                    return Ty::Error;
                }
                if !args.is_empty() {
                    self.diags.push(
                        Diagnostic::error(codes::WRONG_ARG_COUNT, "`toString` takes no arguments")
                            .primary(e.span, "wrong number of arguments"),
                    );
                }
                Ty::Str
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
