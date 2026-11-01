//! Lowering from the typed HIR to MIR.
//!
//! Expressions lower to operands, emitting statements into the current block. Code after a
//! diverging expression (`return`, `break`, `panic`, …) goes into a fresh block with no
//! predecessors, which keeps lowering simple; such blocks end in `unreachable`.

use crate::mir::{
    self, BlockId, Callee, Const, Loc, Local, LocalDecl, Operand, Place, Rvalue, Terminator, panics,
};
use ppl_diag::{Diagnostic, SourceFile, Span, codes};
use ppl_sema::hir::{self, Builtin, Expr, ExprKind, FnId, Stmt, Var};
use ppl_sema::{IntTy, Ty, Types};
use ppl_syntax::ast;
use std::collections::HashSet;

pub fn lower(
    program: &hir::Program,
    types: &Types,
    file: &SourceFile,
) -> (mir::Program, Vec<Diagnostic>) {
    let mut cx = Cx {
        program,
        types,
        file,
        diags: Vec::new(),
        reported: HashSet::new(),
    };
    let fns = (0..program.fns.len())
        .map(|i| cx.function(FnId(i as u32)))
        .collect();
    let globals = program
        .globals
        .iter()
        .zip(&types.globals)
        .map(|(g, t)| mir::GlobalDecl {
            name: g.name.clone(),
            ty: t.clone(),
        })
        .collect();
    let structs = program
        .structs
        .iter()
        .map(|s| mir::StructLayout {
            name: s.name.clone(),
            fields: s
                .fields
                .iter()
                .map(|f| (f.name.clone(), f.ty.clone()))
                .collect(),
        })
        .collect();
    let enums = program
        .enums
        .iter()
        .map(|e| mir::EnumLayout {
            name: e.name.clone(),
            variants: e
                .variants
                .iter()
                .map(|v| mir::VariantLayout {
                    name: v.name.clone(),
                    fields: v
                        .fields
                        .iter()
                        .map(|f| (f.name.clone(), f.ty.clone()))
                        .collect(),
                    has_parens: v.has_parens,
                })
                .collect(),
        })
        .collect();
    let mir = mir::Program {
        file: file.name.clone(),
        fns,
        entry: program.entry,
        globals,
        structs,
        enums,
    };
    (mir, cx.diags)
}

struct Cx<'a> {
    program: &'a hir::Program,
    types: &'a Types,
    file: &'a SourceFile,
    diags: Vec<Diagnostic>,
    /// Spans already reported as not implemented (lowering has no such gaps right now).
    #[allow(dead_code)]
    reported: HashSet<(u32, u32)>,
}

impl Cx<'_> {
    #[allow(dead_code)]
    fn not_implemented(&mut self, span: Span, what: &str) {
        if !self.reported.insert((span.start, span.end)) {
            return;
        }
        self.diags.push(
            Diagnostic::error(
                codes::NOT_IMPLEMENTED,
                format!("{what} isn't implemented yet"),
            )
            .primary(span, "not supported by this compiler version"),
        );
    }

    fn function(&mut self, id: FnId) -> mir::Function {
        let f = self.program.func(id);
        let sig = &self.types.sigs[id.0 as usize];
        let local_tys = &self.types.locals[id.0 as usize];
        let mut b = FnBuilder {
            cx: self,
            hir_fn: f,
            fn_id: id,
            locals: Vec::new(),
            blocks: vec![(Vec::new(), None)],
            cur: BlockId(0),
            loops: Vec::new(),
        };
        for (i, (l, t)) in f.locals.iter().zip(local_tys).enumerate() {
            // `this` is passed by reference.
            let is_this = f.this.is_some_and(|(this, _)| this.0 as usize == i);
            let ty = if is_this {
                Ty::Ref(Box::new(t.clone()))
            } else {
                t.clone()
            };
            b.locals.push(LocalDecl {
                ty,
                name: Some(l.name.clone()),
            });
        }
        let value = b.block(&f.body);
        let ret = if f.is_entry {
            Ty::Void
        } else {
            sig.ret.clone()
        };
        let term = match ret {
            Ty::Void | Ty::Never | Ty::Error => Terminator::Return(None),
            _ => Terminator::Return(Some(value)),
        };
        b.terminate(term);
        let blocks = b
            .blocks
            .into_iter()
            .map(|(stmts, term)| mir::BasicBlock {
                stmts,
                term: term.unwrap_or(Terminator::Unreachable),
            })
            .collect();
        mir::Function {
            name: f.name.clone(),
            // A method's `this` reference is its first parameter.
            params: f
                .this
                .map(|(this, _)| Local(this.0))
                .into_iter()
                .chain(f.params.iter().map(|p| Local(p.0)))
                .collect(),
            ret,
            locals: b.locals,
            blocks,
        }
    }
}

/// A bounds check that must run before an element place is used: `idx` against the length
/// of the array at `base`.
#[derive(Clone)]
struct Check {
    idx: Local,
    base: Place,
    span: Span,
}

struct LoopTarget {
    brk: BlockId,
    cont: BlockId,
    result: Option<Place>,
}

struct FnBuilder<'c, 'a> {
    cx: &'c mut Cx<'a>,
    hir_fn: &'a hir::Function,
    fn_id: FnId,
    locals: Vec<LocalDecl>,
    blocks: Vec<(Vec<mir::Statement>, Option<Terminator>)>,
    cur: BlockId,
    loops: Vec<LoopTarget>,
}

fn has_value(t: &Ty) -> bool {
    !matches!(t, Ty::Void | Ty::Never | Ty::Error)
}

fn bin_op(op: ast::BinOp) -> mir::BinOp {
    use ast::BinOp as A;
    use mir::BinOp as M;
    match op {
        A::Add => M::Add,
        A::Sub => M::Sub,
        A::Mul => M::Mul,
        A::Div => M::Div,
        A::Rem => M::Rem,
        A::Pow => M::Pow,
        A::Shl => M::Shl,
        A::Shr => M::Shr,
        A::UShr => M::UShr,
        A::BitAnd => M::BitAnd,
        A::BitOr => M::BitOr,
        A::BitXor => M::BitXor,
        A::Eq => M::Eq,
        A::Ne => M::Ne,
        A::Lt => M::Lt,
        A::Le => M::Le,
        A::Gt => M::Gt,
        A::Ge => M::Ge,
        A::And | A::Or | A::Nullish => unreachable!("lowered as control flow"),
    }
}

impl FnBuilder<'_, '_> {
    fn loc(&self, span: Span) -> Loc {
        let (line, col) = self.cx.file.line_col(span.start);
        Loc {
            line: line as u32,
            col: col as u32,
        }
    }

    fn ty(&self, e: &Expr) -> Ty {
        self.cx.types.expr(e.id).clone()
    }

    fn temp(&mut self, ty: Ty) -> Local {
        self.locals.push(LocalDecl { ty, name: None });
        Local(self.locals.len() as u32 - 1)
    }

    fn new_block(&mut self) -> BlockId {
        self.blocks.push((Vec::new(), None));
        BlockId(self.blocks.len() as u32 - 1)
    }

    fn switch_to(&mut self, b: BlockId) {
        self.cur = b;
    }

    fn assign(&mut self, place: Place, rvalue: Rvalue, span: Span) {
        let loc = self.loc(span);
        self.blocks[self.cur.0 as usize]
            .0
            .push(mir::Statement { place, rvalue, loc });
    }

    /// Ends the current block (unless it already ended).
    fn terminate(&mut self, term: Terminator) {
        let slot = &mut self.blocks[self.cur.0 as usize].1;
        if slot.is_none() {
            *slot = Some(term);
        }
    }

    /// Ends the current block and continues in a fresh, unreachable one.
    fn diverge(&mut self, term: Terminator) {
        self.terminate(term);
        let dead = self.new_block();
        self.switch_to(dead);
    }

    fn place(&self, v: Var) -> Place {
        match v {
            Var::Local(l) if self.hir_fn.this.is_some_and(|(this, _)| this == l) => {
                Place::deref(Local(l.0))
            }
            Var::Local(l) => Place::local(Local(l.0)),
            Var::Global(g) => Place::global(g),
        }
    }

    fn var_ty(&self, v: Var) -> Ty {
        match v {
            Var::Local(l) => self.cx.types.locals[self.fn_id.0 as usize][l.0 as usize].clone(),
            Var::Global(g) => self.cx.types.globals[g.0 as usize].clone(),
        }
    }

    /// The type of an expression's value (after coercion).
    fn ty_of(&self, e: &Expr) -> &Ty {
        self.cx.types.final_ty(e.id)
    }

    fn place_ty(&self, p: &Place) -> Ty {
        let mut t = match p.base {
            mir::Base::Local(l) => self.locals[l.0 as usize].ty.clone(),
            mir::Base::Global(g) => self.cx.types.globals[g.0 as usize].clone(),
            mir::Base::Deref(l) => match &self.locals[l.0 as usize].ty {
                Ty::Ref(inner) => (**inner).clone(),
                other => unreachable!("deref of {other}"),
            },
        };
        for pr in &p.proj {
            t = mir::project(&t, pr, |f| match f {
                mir::FieldRef::Struct(sid, i) => {
                    self.cx.program.struct_def(sid).fields[i].ty.clone()
                }
                mir::FieldRef::Variant(eid, v, i) => {
                    self.cx.program.enum_def(eid).variants[v as usize].fields[i]
                        .ty
                        .clone()
                }
            });
        }
        t
    }

    /// A place holding the value of `e`: variables, field paths and elements directly,
    /// anything else in a temporary. Bounds checks run before it returns.
    fn place_of(&mut self, e: &Expr) -> Place {
        let mut checks = Vec::new();
        let p = self.place_deferred(e, &mut checks);
        self.emit_checks(&checks);
        p
    }

    /// Like [`Self::place_of`], but leaves the bounds checks in `checks` (an assignment checks
    /// after evaluating its value, which might change the array).
    fn place_deferred(&mut self, e: &Expr, checks: &mut Vec<Check>) -> Place {
        match &e.kind {
            ExprKind::Var(v) if self.cx.types.narrowed.contains(&e.id) => self.place(*v).payload(),
            ExprKind::Var(v) => self.place(*v),
            ExprKind::Member { object, .. } if matches!(self.ty_of(object), Ty::Struct(..)) => {
                let i = self.cx.types.fields[&e.id];
                self.place_deferred(object, checks).field(i)
            }
            ExprKind::Member { object, name, .. } if matches!(self.ty_of(object), Ty::Range(_)) => {
                let i = if name == "start" { 0 } else { 1 };
                self.place_deferred(object, checks).field(i)
            }
            ExprKind::Index { object, index } if *self.ty_of(object) != Ty::Str => {
                self.index_place(object, index, checks)
            }
            _ => match self.expr(e) {
                Operand::Copy(p) => p,
                c @ Operand::Const(_) => {
                    let ty = self.ty_of(e).clone();
                    let t = self.temp(ty);
                    self.assign(Place::local(t), Rvalue::Use(c), e.span);
                    Place::local(t)
                }
            },
        }
    }

    /// `object[index]` for tuples (a constant index), fixed and growable arrays.
    fn index_place(&mut self, object: &Expr, index: &Expr, checks: &mut Vec<Check>) -> Place {
        let base = self.place_deferred(object, checks);
        if let Ty::Tuple(_) = self.ty_of(object) {
            let ExprKind::Int(i, _) = index.kind else {
                unreachable!("type-checked tuple index")
            };
            return base.field(i as u32);
        }
        let v = self.expr(index);
        let idx = self.temp(Ty::INT);
        self.assign(Place::local(idx), Rvalue::Use(v), index.span);
        checks.push(Check {
            idx,
            base: base.clone(),
            span: object.span.to(index.span),
        });
        base.index(idx)
    }

    /// Panics with "index out of bounds" unless each index is below its array's length.
    fn emit_checks(&mut self, checks: &[Check]) {
        for c in checks {
            let len = match self.place_ty(&c.base) {
                Ty::Fixed(_, n) => Operand::Const(Const::Int(n as i128, IntTy::I64)),
                _ => self.rvalue_to_temp(
                    Rvalue::ArrayLen(Operand::Copy(c.base.clone())),
                    Ty::INT,
                    c.span,
                ),
            };
            let ok = self.rvalue_to_temp(
                Rvalue::InBounds(Operand::Copy(Place::local(c.idx)), len),
                Ty::Bool,
                c.span,
            );
            self.panic_unless(ok, panics::INDEX, c.span);
        }
    }

    /// Continues if `ok` is true; panics with `msg` otherwise.
    fn panic_unless(&mut self, ok: Operand, msg: &str, span: Span) {
        let pass = self.new_block();
        let fail = self.new_block();
        self.terminate(Terminator::Branch {
            cond: ok,
            then: pass,
            else_: fail,
        });
        self.switch_to(fail);
        let loc = self.loc(span);
        self.terminate(Terminator::Panic {
            msg: Operand::Const(Const::Str(msg.into())),
            loc,
        });
        self.switch_to(pass);
    }

    /// Reads a place; through an array element, into a temporary (later code might shrink or
    /// grow the array).
    fn read_place(&mut self, p: Place, span: Span) -> Operand {
        if p.has_index() {
            let ty = self.place_ty(&p);
            if !has_value(&ty) {
                return Operand::Const(Const::Void);
            }
            return self.rvalue_to_temp(Rvalue::Use(Operand::Copy(p)), ty, span);
        }
        Operand::Copy(p)
    }

    fn target_place(&mut self, t: &hir::Target, checks: &mut Vec<Check>) -> Place {
        match t {
            hir::Target::Var(v) => self.place(*v),
            hir::Target::Field { id, object, .. } => {
                let i = self.cx.types.fields[id];
                self.place_deferred(object, checks).field(i)
            }
            hir::Target::Index { object, index } => self.index_place(object, index, checks),
        }
    }

    fn rvalue_to_temp(&mut self, rv: Rvalue, ty: Ty, span: Span) -> Operand {
        let t = self.temp(ty);
        self.assign(Place::local(t), rv, span);
        Operand::Copy(Place::local(t))
    }

    // ------------------------------------------------------------ blocks

    fn block(&mut self, b: &hir::Block) -> Operand {
        for s in &b.stmts {
            match s {
                Stmt::Let { var, init, span } => {
                    if let Some(init) = init {
                        let v = self.expr(init);
                        if has_value(&self.var_ty(*var)) {
                            let p = self.place(*var);
                            self.assign(p, Rvalue::Use(v), *span);
                        }
                    }
                }
                Stmt::Expr(e) => {
                    self.expr(e);
                }
            }
        }
        match &b.tail {
            Some(t) => self.expr(t),
            None => Operand::Const(Const::Void),
        }
    }

    // ------------------------------------------------------------ expressions

    /// Lowers `e` and applies its recorded coercion.
    fn expr(&mut self, e: &Expr) -> Operand {
        let v = self.expr_inner(e);
        match self.cx.types.coercions.get(&e.id) {
            Some(to) => self.convert(v, &self.ty(e), to, e.span),
            None => v,
        }
    }

    fn convert(&mut self, v: Operand, from: &Ty, to: &Ty, span: Span) -> Operand {
        if from == to || *from == Ty::Never {
            return v;
        }
        if *from == Ty::Null {
            return Operand::Const(Const::Null(to.clone()));
        }
        match (from, to) {
            (Ty::Nullable(a), Ty::Nullable(b)) => {
                // Null stays null; a value converts.
                let res = Place::local(self.temp(to.clone()));
                let is_null = self.rvalue_to_temp(Rvalue::IsNull(v.clone()), Ty::Bool, span);
                let null_bb = self.new_block();
                let some_bb = self.new_block();
                let join = self.new_block();
                self.terminate(Terminator::Branch {
                    cond: is_null,
                    then: null_bb,
                    else_: some_bb,
                });
                self.switch_to(null_bb);
                self.assign(
                    res.clone(),
                    Rvalue::Use(Operand::Const(Const::Null(to.clone()))),
                    span,
                );
                self.terminate(Terminator::Goto(join));
                self.switch_to(some_bb);
                let inner = self.rvalue_to_temp(Rvalue::Unwrap(v), (**a).clone(), span);
                let c = self.convert(inner, a, b, span);
                self.assign(res.clone(), Rvalue::WrapSome(c, to.clone()), span);
                self.terminate(Terminator::Goto(join));
                self.switch_to(join);
                return Operand::Copy(res);
            }
            (_, Ty::Nullable(b)) => {
                let c = self.convert(v, from, b, span);
                return self.rvalue_to_temp(Rvalue::WrapSome(c, to.clone()), to.clone(), span);
            }
            _ => {}
        }
        match (&v, to) {
            (Operand::Const(Const::Int(n, _)), Ty::Int(it)) => Operand::Const(Const::Int(*n, *it)),
            (Operand::Const(Const::Int(n, _)), Ty::Float(ft)) => {
                Operand::Const(Const::Float(*n as f64, *ft))
            }
            _ => self.rvalue_to_temp(Rvalue::Cast(v, to.clone()), to.clone(), span),
        }
    }

    fn expr_inner(&mut self, e: &Expr) -> Operand {
        let ty = self.ty(e);
        match &e.kind {
            ExprKind::Int(v, _) => int_const(*v as i128, &ty),
            ExprKind::Float(v, _) => match ty {
                Ty::Float(ft) => Operand::Const(Const::Float(*v, ft)),
                _ => Operand::Const(Const::Void),
            },
            ExprKind::Str(s) => match ty {
                Ty::Char => Operand::Const(Const::Char(s.chars().next().unwrap_or('\0'))),
                _ => Operand::Const(Const::Str(s.clone())),
            },
            ExprKind::Bool(b) => Operand::Const(Const::Bool(*b)),
            // The coercion to the expected `T?` turns this into `Const::Null`.
            ExprKind::Null => Operand::Const(Const::Void),
            ExprKind::NonNull(inner) => {
                let v = self.expr(inner);
                if !matches!(self.ty_of(inner), Ty::Nullable(_)) {
                    return v;
                }
                let is_null = self.rvalue_to_temp(Rvalue::IsNull(v.clone()), Ty::Bool, e.span);
                let ok =
                    self.rvalue_to_temp(Rvalue::Unary(mir::UnOp::Not, is_null), Ty::Bool, e.span);
                self.panic_unless(ok, panics::NULL_UNWRAP, e.span);
                self.rvalue_to_temp(Rvalue::Unwrap(v), ty, e.span)
            }
            ExprKind::OptChain { tmp, base, rest } => self.opt_chain(*tmp, base, rest, ty, e.span),
            ExprKind::Variant {
                enum_,
                variant,
                args,
            } => {
                let n = self.cx.program.enum_def(*enum_).variants[*variant as usize]
                    .fields
                    .len();
                let mut ops: Vec<Option<Operand>> = vec![None; n];
                if let Some(args) = args {
                    let order = self.cx.types.variant_args[&e.id].clone();
                    // Evaluate in source order, store in declaration order.
                    for (a, slot) in args.iter().zip(order) {
                        let v = self.expr(&a.value);
                        ops[slot as usize] = Some(v);
                    }
                }
                let ops = ops
                    .into_iter()
                    .map(|o| o.expect("every field given"))
                    .collect();
                self.rvalue_to_temp(Rvalue::Variant(*enum_, *variant, ops), ty, e.span)
            }
            ExprKind::Match {
                scrutinee,
                arms,
                switch,
            } => self.match_expr(scrutinee, arms, switch.is_some(), ty, e.span),
            ExprKind::Is { expr, pat } => {
                let place = self.place_of(expr);
                let res = Place::local(self.temp(Ty::Bool));
                let no = self.new_block();
                let join = self.new_block();
                self.match_pattern(pat, &place, no);
                self.assign(
                    res.clone(),
                    Rvalue::Use(Operand::Const(Const::Bool(true))),
                    e.span,
                );
                self.terminate(Terminator::Goto(join));
                self.switch_to(no);
                self.assign(
                    res.clone(),
                    Rvalue::Use(Operand::Const(Const::Bool(false))),
                    e.span,
                );
                self.terminate(Terminator::Goto(join));
                self.switch_to(join);
                Operand::Copy(res)
            }
            ExprKind::Var(v) if self.cx.types.narrowed.contains(&e.id) => {
                Operand::Copy(self.place(*v).payload())
            }
            ExprKind::Var(v) => Operand::Copy(self.place(*v)),
            ExprKind::Fn(_) | ExprKind::Builtin(_) | ExprKind::Error => Operand::Const(Const::Void),
            ExprKind::Unary(op, inner) => {
                if *op == ast::UnOp::Neg
                    && let ExprKind::Int(v, _) = inner.kind
                {
                    return int_const(-(v as i128), &ty);
                }
                let v = self.expr(inner);
                let op = match op {
                    ast::UnOp::Neg => mir::UnOp::Neg,
                    ast::UnOp::Not => mir::UnOp::Not,
                    ast::UnOp::BitNot => mir::UnOp::BitNot,
                    ast::UnOp::Plus => return v,
                    _ => unreachable!("resolved earlier"),
                };
                self.rvalue_to_temp(Rvalue::Unary(op, v), ty, e.span)
            }
            ExprKind::Binary(op, l, r) => self.binary(e, *op, l, r, ty),
            ExprKind::Assign { op, target, value } => {
                let mut checks = Vec::new();
                let place = self.target_place(target, &mut checks);
                match op {
                    None => {
                        let v = self.expr(value);
                        self.emit_checks(&checks);
                        self.assign(place, Rvalue::Use(v), e.span);
                    }
                    Some(ast::BinOp::Nullish) => {
                        // `x ??= v`: assign only if `x` is null.
                        self.emit_checks(&checks);
                        let assign_bb = self.new_block();
                        let join = self.new_block();
                        let is_null = self.rvalue_to_temp(
                            Rvalue::IsNull(Operand::Copy(place.clone())),
                            Ty::Bool,
                            e.span,
                        );
                        self.terminate(Terminator::Branch {
                            cond: is_null,
                            then: assign_bb,
                            else_: join,
                        });
                        self.switch_to(assign_bb);
                        let v = self.expr(value);
                        self.emit_checks(&checks);
                        self.assign(place.clone(), Rvalue::Use(v), e.span);
                        self.terminate(Terminator::Goto(join));
                        self.switch_to(join);
                    }
                    Some(op @ (ast::BinOp::And | ast::BinOp::Or)) => {
                        // `x &&= v`: assign only if `x` is true (`||=`: if false).
                        self.emit_checks(&checks);
                        let assign_bb = self.new_block();
                        let join = self.new_block();
                        let cur = Operand::Copy(place.clone());
                        let (t, f) = if *op == ast::BinOp::And {
                            (assign_bb, join)
                        } else {
                            (join, assign_bb)
                        };
                        self.terminate(Terminator::Branch {
                            cond: cur,
                            then: t,
                            else_: f,
                        });
                        self.switch_to(assign_bb);
                        let v = self.expr(value);
                        self.emit_checks(&checks);
                        self.assign(place.clone(), Rvalue::Use(v), e.span);
                        self.terminate(Terminator::Goto(join));
                        self.switch_to(join);
                    }
                    Some(op) => {
                        let v = self.expr(value);
                        self.emit_checks(&checks);
                        self.assign(
                            place.clone(),
                            Rvalue::Binary(bin_op(*op), Operand::Copy(place), v),
                            e.span,
                        );
                    }
                }
                Operand::Const(Const::Void)
            }
            ExprKind::IncDec {
                target,
                inc,
                prefix,
            } => {
                let mut checks = Vec::new();
                let place = self.target_place(target, &mut checks);
                self.emit_checks(&checks);
                let vt = self.place_ty(&place);
                let Ty::Int(it) = vt else {
                    return Operand::Const(Const::Void);
                };
                let old = self.rvalue_to_temp(
                    Rvalue::Use(Operand::Copy(place.clone())),
                    vt.clone(),
                    e.span,
                );
                let op = if *inc {
                    mir::BinOp::Add
                } else {
                    mir::BinOp::Sub
                };
                let new = self.rvalue_to_temp(
                    Rvalue::Binary(op, old.clone(), Operand::Const(Const::Int(1, it))),
                    vt,
                    e.span,
                );
                self.assign(place, Rvalue::Use(new.clone()), e.span);
                if *prefix { new } else { old }
            }
            ExprKind::Call { callee, args } => self.call(e, callee, args, ty),
            ExprKind::Member { object, .. } if *self.ty_of(object) == Ty::Str => {
                let s = self.expr(object);
                self.rvalue_to_temp(Rvalue::StrLen(s), ty, e.span)
            }
            ExprKind::Member { object, .. } if matches!(self.ty_of(object), Ty::Array(_)) => {
                let a = self.expr(object);
                self.rvalue_to_temp(Rvalue::ArrayLen(a), ty, e.span)
            }
            ExprKind::Member { object, .. } if matches!(self.ty_of(object), Ty::Fixed(..)) => {
                let Ty::Fixed(_, n) = self.ty_of(object).clone() else {
                    unreachable!()
                };
                self.expr(object);
                Operand::Const(Const::Int(n as i128, IntTy::I64))
            }
            ExprKind::Member { .. } => {
                let place = self.place_of(e);
                self.read_place(place, e.span)
            }
            ExprKind::Object { fields, .. } => {
                let (sid, order) = self.cx.types.objects[&e.id].clone();
                let mut ops: Vec<Option<Operand>> = vec![None; order.len()];
                // Evaluate in source order, store in declaration order.
                for (f, slot) in fields.iter().zip(&order) {
                    let v = self.expr(&f.value);
                    ops[*slot as usize] = Some(v);
                }
                let ops = ops
                    .into_iter()
                    .map(|o| o.expect("every field initialized"))
                    .collect();
                self.rvalue_to_temp(Rvalue::Aggregate(sid, ops), ty, e.span)
            }
            ExprKind::Index { object, index } if *self.ty_of(object) == Ty::Str => {
                let s = self.expr(object);
                let i = self.expr(index);
                self.rvalue_to_temp(Rvalue::StrCharAt(s, i), ty, e.span)
            }
            ExprKind::Index { .. } => {
                let place = self.place_of(e);
                self.read_place(place, e.span)
            }
            ExprKind::Array(elems) => {
                let ops: Vec<Operand> = elems.iter().map(|x| self.expr(x)).collect();
                match &ty {
                    Ty::Array(t) => {
                        let t = (**t).clone();
                        self.rvalue_to_temp(Rvalue::ArrayNew(t, ops), ty, e.span)
                    }
                    Ty::Tuple(_) | Ty::Fixed(..) => {
                        self.rvalue_to_temp(Rvalue::Tuple(ty.clone(), ops), ty, e.span)
                    }
                    _ => Operand::Const(Const::Void),
                }
            }
            ExprKind::Repeat { value, count } => {
                let v = self.expr(value);
                if !matches!(ty, Ty::Fixed(..)) {
                    return Operand::Const(Const::Void);
                }
                self.rvalue_to_temp(Rvalue::Repeat(v, *count), ty, e.span)
            }
            ExprKind::Range { start, end } => {
                let a = self.expr(start);
                let b = self.expr(end);
                let Ty::Range(it) = ty else {
                    return Operand::Const(Const::Void);
                };
                let one = Operand::Const(Const::Int(1, it));
                self.rvalue_to_temp(Rvalue::Tuple(ty.clone(), vec![a, b, one]), ty, e.span)
            }
            ExprKind::ForOf { var, iter, body } => {
                self.for_of(*var, iter, body, e.span);
                Operand::Const(Const::Void)
            }
            ExprKind::Template { quasis, parts } => self.template(quasis, parts, e.span),
            ExprKind::If { cond, then, else_ } => {
                self.if_expr(cond, then, else_.as_deref(), ty, e.span)
            }
            ExprKind::Block(b) => self.block(b),
            ExprKind::Loop(b) => {
                let body = self.new_block();
                let exit = self.new_block();
                let result = has_value(&ty).then(|| Place::local(self.temp(ty.clone())));
                self.terminate(Terminator::Goto(body));
                self.switch_to(body);
                self.loops.push(LoopTarget {
                    brk: exit,
                    cont: body,
                    result: result.clone(),
                });
                self.block(b);
                self.loops.pop();
                self.terminate(Terminator::Goto(body));
                self.switch_to(exit);
                match result {
                    Some(p) => Operand::Copy(p),
                    None => Operand::Const(Const::Void),
                }
            }
            ExprKind::While {
                cond,
                body,
                update,
                test_first,
            } => {
                let cond_bb = self.new_block();
                let body_bb = self.new_block();
                let update_bb = update.as_ref().map(|_| self.new_block());
                let exit = self.new_block();
                self.terminate(Terminator::Goto(if *test_first {
                    cond_bb
                } else {
                    body_bb
                }));
                self.switch_to(cond_bb);
                let c = self.expr(cond);
                self.terminate(Terminator::Branch {
                    cond: c,
                    then: body_bb,
                    else_: exit,
                });
                self.switch_to(body_bb);
                let cont = update_bb.unwrap_or(cond_bb);
                self.loops.push(LoopTarget {
                    brk: exit,
                    cont,
                    result: None,
                });
                self.expr(body);
                self.loops.pop();
                self.terminate(Terminator::Goto(cont));
                if let (Some(u), Some(ubb)) = (update, update_bb) {
                    self.switch_to(ubb);
                    self.expr(u);
                    self.terminate(Terminator::Goto(cond_bb));
                }
                self.switch_to(exit);
                Operand::Const(Const::Void)
            }
            ExprKind::Break(value) => {
                let v = value.as_ref().map(|v| (self.expr(v), self.ty(v)));
                let target = self.loops.last().expect("resolver checked break placement");
                let (brk, result) = (target.brk, target.result.clone());
                if let (Some((v, vt)), Some(r)) = (v, result)
                    && vt != Ty::Never
                {
                    self.assign(r, Rvalue::Use(v), e.span);
                }
                self.diverge(Terminator::Goto(brk));
                Operand::Const(Const::Void)
            }
            ExprKind::Continue => {
                let cont = self
                    .loops
                    .last()
                    .expect("resolver checked continue placement")
                    .cont;
                self.diverge(Terminator::Goto(cont));
                Operand::Const(Const::Void)
            }
            ExprKind::Return(value) => {
                let v = value.as_ref().map(|v| (self.expr(v), self.ty(v)));
                let ret_has_value = !self.hir_fn.is_entry
                    && has_value(&self.cx.types.sigs[self.fn_id.0 as usize].ret);
                let term = match v {
                    Some((v, _)) if ret_has_value => Terminator::Return(Some(v)),
                    _ => Terminator::Return(None),
                };
                self.diverge(term);
                Operand::Const(Const::Void)
            }
            ExprKind::As(inner, to) => {
                let from = self.ty(inner);
                let v = self.expr(inner);
                if from == *to {
                    return v;
                }
                if let (Operand::Const(Const::Int(n, _)), Ty::Int(it)) = (&v, to)
                    && it.contains(*n)
                {
                    return Operand::Const(Const::Int(*n, *it));
                }
                self.rvalue_to_temp(Rvalue::Cast(v, to.clone()), to.clone(), e.span)
            }
        }
    }

    /// `for (var of iter) body` over a range, an array, a fixed array or a string.
    fn for_of(&mut self, var: hir::LocalId, iter: &Expr, body: &Expr, span: Span) {
        let it_ty = self.ty_of(iter).clone();
        // `for (i of a..b)`: a plain counting loop (step 1 can't overflow past `b`).
        let literal = match (&iter.kind, &it_ty) {
            (ExprKind::Range { start, end }, Ty::Range(it)) => {
                let a = self.expr(start);
                let b = self.expr(end);
                let end = Place::local(self.temp(Ty::Int(*it)));
                self.assign(end.clone(), Rvalue::Use(b), iter.span);
                Some((a, end))
            }
            _ => None,
        };
        if !has_value(&it_ty) {
            self.expr(iter);
            return;
        }
        // The iterated value (unused for a literal range).
        let src = match literal {
            Some(_) => Place::local(Local(0)),
            None => Place::local(self.temp(it_ty.clone())),
        };
        let start = match &literal {
            Some((a, _)) => a.clone(),
            None => {
                let v = self.expr(iter);
                self.assign(src.clone(), Rvalue::Use(v), iter.span);
                match &it_ty {
                    Ty::Range(_) => Operand::Copy(src.field(0)),
                    _ => Operand::Const(Const::Int(0, IntTy::I64)),
                }
            }
        };
        let var_place = self.place(hir::Var::Local(var));
        let var_has_value = has_value(&self.var_ty(hir::Var::Local(var)));
        let counter_ty = match &it_ty {
            Ty::Range(it) => Ty::Int(*it),
            _ => Ty::INT,
        };
        let i = Place::local(self.temp(counter_ty.clone()));
        self.assign(i.clone(), Rvalue::Use(start), span);
        let cond_bb = self.new_block();
        let body_bb = self.new_block();
        let update_bb = self.new_block();
        let exit = self.new_block();
        self.terminate(Terminator::Goto(cond_bb));

        self.switch_to(cond_bb);
        let end = match &it_ty {
            Ty::Range(_) if literal.is_some() => Operand::Copy(literal.clone().unwrap().1),
            Ty::Range(_) => Operand::Copy(src.field(1)),
            Ty::Fixed(_, n) => Operand::Const(Const::Int(*n as i128, IntTy::I64)),
            Ty::Str => {
                self.rvalue_to_temp(Rvalue::StrLen(Operand::Copy(src.clone())), Ty::INT, span)
            }
            _ => self.rvalue_to_temp(Rvalue::ArrayLen(Operand::Copy(src.clone())), Ty::INT, span),
        };
        let c = self.rvalue_to_temp(
            Rvalue::Binary(mir::BinOp::Lt, Operand::Copy(i.clone()), end),
            Ty::Bool,
            span,
        );
        self.terminate(Terminator::Branch {
            cond: c,
            then: body_bb,
            else_: exit,
        });

        self.switch_to(body_bb);
        if var_has_value {
            let item = match &it_ty {
                Ty::Range(_) => Rvalue::Use(Operand::Copy(i.clone())),
                Ty::Str => Rvalue::StrCharAt(Operand::Copy(src.clone()), Operand::Copy(i.clone())),
                _ => {
                    let Place {
                        base: mir::Base::Local(il),
                        ..
                    } = i
                    else {
                        unreachable!()
                    };
                    Rvalue::Use(Operand::Copy(src.index(il)))
                }
            };
            self.assign(var_place, item, span);
        }
        self.loops.push(LoopTarget {
            brk: exit,
            cont: update_bb,
            result: None,
        });
        self.expr(body);
        self.loops.pop();
        self.terminate(Terminator::Goto(update_bb));

        self.switch_to(update_bb);
        let step = match &it_ty {
            Ty::Range(it) if literal.is_some() => Operand::Const(Const::Int(1, *it)),
            Ty::Range(_) => Operand::Copy(src.field(2)),
            _ => Operand::Const(Const::Int(1, IntTy::I64)),
        };
        if let (Ty::Range(_), None) = (&it_ty, &literal) {
            // Stop instead of overflowing past the type's maximum: `(0..255u8).step(10)`.
            let ov = self.rvalue_to_temp(
                Rvalue::AddOverflows(Operand::Copy(i.clone()), step.clone()),
                Ty::Bool,
                span,
            );
            let next = self.new_block();
            self.terminate(Terminator::Branch {
                cond: ov,
                then: exit,
                else_: next,
            });
            self.switch_to(next);
        }
        self.assign(
            i.clone(),
            Rvalue::Binary(mir::BinOp::Add, Operand::Copy(i), step),
            span,
        );
        self.terminate(Terminator::Goto(cond_bb));
        self.switch_to(exit);
    }

    /// Methods of built-in types: `xs.push(v)`, `xs.pop()`, `r.step(n)`.
    fn builtin_method(
        &mut self,
        e: &Expr,
        object: &Expr,
        name: &str,
        args: &[Expr],
        ty: Ty,
    ) -> Operand {
        match name {
            "push" => {
                let a = self.expr(object);
                for x in args {
                    let v = self.expr(x);
                    let t = self.temp(Ty::Void);
                    self.assign(Place::local(t), Rvalue::ArrayPush(a.clone(), v), e.span);
                }
                Operand::Const(Const::Void)
            }
            "pop" => {
                let a = self.expr(object);
                self.rvalue_to_temp(Rvalue::ArrayPop(a), ty, e.span)
            }
            "step" => {
                let r = self.place_of(object);
                let n = self.expr(&args[0]);
                let Ty::Range(it) = ty else {
                    return Operand::Const(Const::Void);
                };
                let ok = self.rvalue_to_temp(
                    Rvalue::Binary(mir::BinOp::Gt, n.clone(), Operand::Const(Const::Int(0, it))),
                    Ty::Bool,
                    e.span,
                );
                self.panic_unless(ok, panics::RANGE_STEP, args[0].span);
                let parts = vec![Operand::Copy(r.field(0)), Operand::Copy(r.field(1)), n];
                self.rvalue_to_temp(Rvalue::Tuple(ty.clone(), parts), ty, e.span)
            }
            _ => unreachable!("type-checked builtin method {name}"),
        }
    }

    fn binary(&mut self, e: &Expr, op: ast::BinOp, l: &Expr, r: &Expr, ty: Ty) -> Operand {
        match op {
            ast::BinOp::And | ast::BinOp::Or => {
                let res = Place::local(self.temp(Ty::Bool));
                let lv = self.expr(l);
                let rhs_bb = self.new_block();
                let short_bb = self.new_block();
                let join = self.new_block();
                let is_and = op == ast::BinOp::And;
                let (t, f) = if is_and {
                    (rhs_bb, short_bb)
                } else {
                    (short_bb, rhs_bb)
                };
                self.terminate(Terminator::Branch {
                    cond: lv,
                    then: t,
                    else_: f,
                });
                self.switch_to(rhs_bb);
                let rv = self.expr(r);
                self.assign(res.clone(), Rvalue::Use(rv), e.span);
                self.terminate(Terminator::Goto(join));
                self.switch_to(short_bb);
                self.assign(
                    res.clone(),
                    Rvalue::Use(Operand::Const(Const::Bool(!is_and))),
                    e.span,
                );
                self.terminate(Terminator::Goto(join));
                self.switch_to(join);
                Operand::Copy(res)
            }
            ast::BinOp::Nullish => self.nullish(l, r, ty, e.span),
            ast::BinOp::Eq | ast::BinOp::Ne
                if matches!(l.kind, ExprKind::Null) || matches!(r.kind, ExprKind::Null) =>
            {
                // `x == null` / `x != null`: a null check.
                let other = if matches!(l.kind, ExprKind::Null) {
                    r
                } else {
                    l
                };
                let v = self.expr(other);
                if !matches!(self.ty_of(other), Ty::Nullable(_)) {
                    // `null == null`
                    return Operand::Const(Const::Bool(op == ast::BinOp::Eq));
                }
                let n = self.rvalue_to_temp(Rvalue::IsNull(v), Ty::Bool, e.span);
                if op == ast::BinOp::Eq {
                    n
                } else {
                    self.rvalue_to_temp(Rvalue::Unary(mir::UnOp::Not, n), Ty::Bool, e.span)
                }
            }
            _ => {
                let lv = self.expr(l);
                let rv = self.expr(r);
                self.rvalue_to_temp(Rvalue::Binary(bin_op(op), lv, rv), ty, e.span)
            }
        }
    }

    /// `match`: the arms are tried in order; each tests its pattern against the scrutinee,
    /// binds, checks its guard, and jumps to the next arm on failure.
    fn match_expr(
        &mut self,
        scrutinee: &Expr,
        arms: &[hir::Arm],
        switch: bool,
        ty: Ty,
        span: Span,
    ) -> Operand {
        let mut place = self.place_of(scrutinee);
        if place.has_index() || !matches!(scrutinee.kind, ExprKind::Var(_)) {
            // A stable copy: arm bodies may change the array or variable it came from.
            let st = self.place_ty(&place);
            let t = Place::local(self.temp(st));
            self.assign(t.clone(), Rvalue::Use(Operand::Copy(place)), scrutinee.span);
            place = t;
        }
        let res = has_value(&ty).then(|| Place::local(self.temp(ty.clone())));
        let join = self.new_block();
        for arm in arms {
            let next = self.new_block();
            self.match_pattern(&arm.pat, &place, next);
            if let Some(g) = &arm.guard {
                let c = self.expr(g);
                let body = self.new_block();
                self.terminate(Terminator::Branch {
                    cond: c,
                    then: body,
                    else_: next,
                });
                self.switch_to(body);
            }
            let v = self.expr(&arm.body);
            if let Some(r) = &res
                && self.ty(&arm.body) != Ty::Never
            {
                self.assign(r.clone(), Rvalue::Use(v), arm.body.span);
            }
            self.terminate(Terminator::Goto(join));
            self.switch_to(next);
        }
        // Exhaustiveness was checked: no value gets here (except past a switch statement).
        let _ = span;
        if switch {
            self.terminate(Terminator::Goto(join));
        } else {
            self.terminate(Terminator::Unreachable);
        }
        self.switch_to(join);
        match res {
            Some(p) => Operand::Copy(p),
            None => Operand::Const(Const::Void),
        }
    }

    /// Branches to `fail` unless the value at `place` matches `p`; binds as it goes (a later
    /// failure leaves some bindings set, which is harmless: they're only read after a match).
    fn match_pattern(&mut self, p: &hir::Pattern, place: &Place, fail: BlockId) {
        use hir::PatKind;
        let pty = self.place_ty(place);
        if p.looks_inside(&pty) {
            let n = self.rvalue_to_temp(
                Rvalue::IsNull(Operand::Copy(place.clone())),
                Ty::Bool,
                p.span,
            );
            let some = self.rvalue_to_temp(Rvalue::Unary(mir::UnOp::Not, n), Ty::Bool, p.span);
            self.branch_unless(some, fail);
            return self.match_pattern(p, &place.payload(), fail);
        }
        match &p.kind {
            PatKind::Wild => {}
            PatKind::Bind(l) => self.bind(*l, place, p.span),
            PatKind::As(l, sub) => {
                self.bind(*l, place, p.span);
                self.match_pattern(sub, place, fail);
            }
            PatKind::Type { bind, fields, .. } => {
                if let Some(l) = bind {
                    self.bind(*l, place, p.span);
                }
                if let Some(fps) = fields {
                    let idx = self.cx.types.pat_fields[&p.id].clone();
                    for (f, i) in fps.iter().zip(idx) {
                        self.match_pattern(&f.pat, &place.field(i), fail);
                    }
                }
            }
            PatKind::Or(alts) => {
                let matched = self.new_block();
                for (k, alt) in alts.iter().enumerate() {
                    let next = if k + 1 == alts.len() {
                        fail
                    } else {
                        self.new_block()
                    };
                    self.match_pattern(alt, place, next);
                    self.terminate(Terminator::Goto(matched));
                    self.switch_to(next);
                }
                self.switch_to(matched);
            }
            PatKind::Lit(hir::LitPat::Null) => {
                let n = self.rvalue_to_temp(
                    Rvalue::IsNull(Operand::Copy(place.clone())),
                    Ty::Bool,
                    p.span,
                );
                self.branch_unless(n, fail);
            }
            PatKind::Lit(l) => {
                let c = lit_const(l, &pty);
                self.compare_or_fail(mir::BinOp::Eq, place, c, fail, p.span);
            }
            PatKind::Range(lo, hi) => {
                let (lo, hi) = (lit_const(lo, &pty), lit_const(hi, &pty));
                self.compare_or_fail(mir::BinOp::Ge, place, lo, fail, p.span);
                // `0..256` on a `u8`: the end is past the type's range, so only `>=` matters.
                let past_max =
                    matches!((&hi, &pty), (Const::Int(v, _), Ty::Int(it)) if *v > it.max());
                if !past_max {
                    self.compare_or_fail(mir::BinOp::Lt, place, hi, fail, p.span);
                }
            }
            PatKind::Variant { args, .. } => {
                let (_, vi) = self.cx.types.variants[&p.id];
                let tag = self.rvalue_to_temp(
                    Rvalue::Tag(Operand::Copy(place.clone())),
                    Ty::Int(IntTy::U32),
                    p.span,
                );
                let eq = self.rvalue_to_temp(
                    Rvalue::Binary(
                        mir::BinOp::Eq,
                        tag,
                        Operand::Const(Const::Int(vi as i128, IntTy::U32)),
                    ),
                    Ty::Bool,
                    p.span,
                );
                self.branch_unless(eq, fail);
                let subs: Vec<(&hir::Pattern, u32)> = match args {
                    hir::VariantPatArgs::None => Vec::new(),
                    hir::VariantPatArgs::Positional(ps) => {
                        ps.iter().enumerate().map(|(i, s)| (s, i as u32)).collect()
                    }
                    hir::VariantPatArgs::Named(fps) => {
                        let idx = &self.cx.types.pat_fields[&p.id];
                        fps.iter().zip(idx).map(|(f, i)| (&f.pat, *i)).collect()
                    }
                };
                for (sub, i) in subs {
                    self.match_pattern(sub, &place.variant_field(vi, i), fail);
                }
            }
            PatKind::Struct { fields, .. } => {
                let idx = self.cx.types.pat_fields[&p.id].clone();
                for (f, i) in fields.iter().zip(idx) {
                    self.match_pattern(&f.pat, &place.field(i), fail);
                }
            }
            PatKind::Slice {
                prefix,
                rest,
                suffix,
            } => self.match_slice(place, &pty, prefix, *rest, suffix, fail, p.span),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn match_slice(
        &mut self,
        place: &Place,
        pty: &Ty,
        prefix: &[hir::Pattern],
        rest: Option<Option<hir::LocalId>>,
        suffix: &[hir::Pattern],
        fail: BlockId,
        span: Span,
    ) {
        let n = prefix.len() + suffix.len();
        match pty {
            Ty::Tuple(_) => {
                for (i, sub) in prefix.iter().chain(suffix).enumerate() {
                    self.match_pattern(sub, &place.field(i as u32), fail);
                }
            }
            Ty::Fixed(..) => {
                for (i, sub) in prefix.iter().chain(suffix).enumerate() {
                    let idx = self.const_index(i as i128, span);
                    self.match_pattern(sub, &place.index(idx), fail);
                }
            }
            _ => {
                // A growable array: check the length, then the elements.
                let len = self.rvalue_to_temp(
                    Rvalue::ArrayLen(Operand::Copy(place.clone())),
                    Ty::INT,
                    span,
                );
                let op = if rest.is_some() {
                    mir::BinOp::Ge
                } else {
                    mir::BinOp::Eq
                };
                let ok = self.rvalue_to_temp(
                    Rvalue::Binary(
                        op,
                        len.clone(),
                        Operand::Const(Const::Int(n as i128, IntTy::I64)),
                    ),
                    Ty::Bool,
                    span,
                );
                self.branch_unless(ok, fail);
                for (i, sub) in prefix.iter().enumerate() {
                    let idx = self.const_index(i as i128, span);
                    self.match_pattern(sub, &place.index(idx), fail);
                }
                // Suffix element j sits at `len - suffix.len() + j`.
                let base = self.rvalue_to_temp(
                    Rvalue::Binary(
                        mir::BinOp::Sub,
                        len.clone(),
                        Operand::Const(Const::Int(suffix.len() as i128, IntTy::I64)),
                    ),
                    Ty::INT,
                    span,
                );
                for (j, sub) in suffix.iter().enumerate() {
                    let at = self.temp(Ty::INT);
                    self.assign(
                        Place::local(at),
                        Rvalue::Binary(
                            mir::BinOp::Add,
                            base.clone(),
                            Operand::Const(Const::Int(j as i128, IntTy::I64)),
                        ),
                        span,
                    );
                    self.match_pattern(sub, &place.index(at), fail);
                }
                if let Some(Some(l)) = rest {
                    let start = Operand::Const(Const::Int(prefix.len() as i128, IntTy::I64));
                    let dst = self.place(hir::Var::Local(l));
                    self.assign(
                        dst,
                        Rvalue::ArraySlice(Operand::Copy(place.clone()), start, base),
                        span,
                    );
                }
            }
        }
    }

    /// A local holding a constant index (for `Proj::Index`).
    fn const_index(&mut self, i: i128, span: Span) -> Local {
        let l = self.temp(Ty::INT);
        self.assign(
            Place::local(l),
            Rvalue::Use(Operand::Const(Const::Int(i, IntTy::I64))),
            span,
        );
        l
    }

    fn bind(&mut self, l: hir::LocalId, place: &Place, span: Span) {
        if has_value(&self.place_ty(place)) {
            let dst = self.place(hir::Var::Local(l));
            self.assign(dst, Rvalue::Use(Operand::Copy(place.clone())), span);
        }
    }

    /// Continues in a fresh block if `ok`, otherwise jumps to `fail`.
    fn branch_unless(&mut self, ok: Operand, fail: BlockId) {
        let pass = self.new_block();
        self.terminate(Terminator::Branch {
            cond: ok,
            then: pass,
            else_: fail,
        });
        self.switch_to(pass);
    }

    fn compare_or_fail(
        &mut self,
        op: mir::BinOp,
        place: &Place,
        c: Const,
        fail: BlockId,
        span: Span,
    ) {
        let ok = self.rvalue_to_temp(
            Rvalue::Binary(op, Operand::Copy(place.clone()), Operand::Const(c)),
            Ty::Bool,
            span,
        );
        self.branch_unless(ok, fail);
    }

    /// `l ?? r`: `r` only when `l` is null.
    fn nullish(&mut self, l: &Expr, r: &Expr, ty: Ty, span: Span) -> Operand {
        let lt = self.ty_of(l).clone();
        let a = self.expr(l);
        if !matches!(lt, Ty::Nullable(_)) {
            if lt == Ty::Null {
                return self.expr(r);
            }
            return a;
        }
        let res = has_value(&ty).then(|| Place::local(self.temp(ty.clone())));
        let is_null = self.rvalue_to_temp(Rvalue::IsNull(a.clone()), Ty::Bool, span);
        let rhs_bb = self.new_block();
        let some_bb = self.new_block();
        let join = self.new_block();
        self.terminate(Terminator::Branch {
            cond: is_null,
            then: rhs_bb,
            else_: some_bb,
        });
        self.switch_to(some_bb);
        if let Some(res) = &res {
            let val = if matches!(ty, Ty::Nullable(_)) {
                Rvalue::Use(a)
            } else {
                Rvalue::Unwrap(a)
            };
            self.assign(res.clone(), val, span);
        }
        self.terminate(Terminator::Goto(join));
        self.switch_to(rhs_bb);
        let v = self.expr(r);
        if let Some(res) = &res
            && self.ty(r) != Ty::Never
        {
            self.assign(res.clone(), Rvalue::Use(v), span);
        }
        self.terminate(Terminator::Goto(join));
        self.switch_to(join);
        match res {
            Some(p) => Operand::Copy(p),
            None => Operand::Const(Const::Void),
        }
    }

    /// `base?.rest`: null if `base` is, otherwise `rest` with `tmp` = the value.
    fn opt_chain(
        &mut self,
        tmp: hir::LocalId,
        base: &Expr,
        rest: &Expr,
        ty: Ty,
        span: Span,
    ) -> Operand {
        let bt = self.ty_of(base).clone();
        let b = self.expr(base);
        let tmp_place = self.place(hir::Var::Local(tmp));
        if !matches!(bt, Ty::Nullable(_)) {
            if has_value(&bt) {
                self.assign(tmp_place, Rvalue::Use(b), span);
            }
            return self.expr(rest);
        }
        let res = has_value(&ty).then(|| Place::local(self.temp(ty.clone())));
        let is_null = self.rvalue_to_temp(Rvalue::IsNull(b.clone()), Ty::Bool, span);
        let null_bb = self.new_block();
        let some_bb = self.new_block();
        let join = self.new_block();
        self.terminate(Terminator::Branch {
            cond: is_null,
            then: null_bb,
            else_: some_bb,
        });
        self.switch_to(some_bb);
        self.assign(tmp_place, Rvalue::Unwrap(b), span);
        let r = self.expr(rest);
        let rt = self.ty_of(rest).clone();
        if let Some(res) = &res
            && rt != Ty::Never
        {
            let val = if rt == ty {
                Rvalue::Use(r)
            } else {
                Rvalue::WrapSome(r, ty.clone())
            };
            self.assign(res.clone(), val, span);
        }
        self.terminate(Terminator::Goto(join));
        self.switch_to(null_bb);
        if let Some(res) = &res {
            self.assign(
                res.clone(),
                Rvalue::Use(Operand::Const(Const::Null(ty.clone()))),
                span,
            );
        }
        self.terminate(Terminator::Goto(join));
        self.switch_to(join);
        match res {
            Some(p) => Operand::Copy(p),
            None => Operand::Const(Const::Void),
        }
    }

    fn if_expr(
        &mut self,
        cond: &Expr,
        then: &Expr,
        else_: Option<&Expr>,
        ty: Ty,
        span: Span,
    ) -> Operand {
        let c = self.expr(cond);
        let then_bb = self.new_block();
        let else_bb = self.new_block();
        let join = self.new_block();
        let result = has_value(&ty).then(|| Place::local(self.temp(ty.clone())));
        self.terminate(Terminator::Branch {
            cond: c,
            then: then_bb,
            else_: else_bb,
        });
        for (bb, branch) in [(then_bb, Some(then)), (else_bb, else_)] {
            self.switch_to(bb);
            match branch {
                Some(branch) => {
                    let v = self.expr(branch);
                    let bt = self.ty_of(branch).clone();
                    if let Some(r) = result.clone()
                        && bt != Ty::Never
                    {
                        // Without `else`, the `then` value becomes `T?`.
                        let v = if else_.is_none() {
                            self.convert(v, &bt, &ty, branch.span)
                        } else {
                            v
                        };
                        self.assign(r, Rvalue::Use(v), branch.span);
                    }
                }
                None => {
                    if let Some(r) = result.clone() {
                        self.assign(
                            r,
                            Rvalue::Use(Operand::Const(Const::Null(ty.clone()))),
                            span,
                        );
                    }
                }
            }
            self.terminate(Terminator::Goto(join));
        }
        self.switch_to(join);
        match result {
            Some(p) => Operand::Copy(p),
            None => Operand::Const(Const::Void),
        }
    }

    /// Converts a value to its string form (no-op for strings).
    fn stringify(&mut self, v: Operand, t: &Ty, span: Span) -> Operand {
        if *t == Ty::Str {
            return v;
        }
        if let Ty::Nullable(inner) = t {
            // "null", or the value's string form.
            let res = Place::local(self.temp(Ty::Str));
            let is_null = self.rvalue_to_temp(Rvalue::IsNull(v.clone()), Ty::Bool, span);
            let null_bb = self.new_block();
            let some_bb = self.new_block();
            let join = self.new_block();
            self.terminate(Terminator::Branch {
                cond: is_null,
                then: null_bb,
                else_: some_bb,
            });
            self.switch_to(null_bb);
            self.assign(
                res.clone(),
                Rvalue::Use(Operand::Const(Const::Str("null".into()))),
                span,
            );
            self.terminate(Terminator::Goto(join));
            self.switch_to(some_bb);
            let inner_v = self.rvalue_to_temp(Rvalue::Unwrap(v), (**inner).clone(), span);
            let s = self.stringify(inner_v, inner, span);
            self.assign(res.clone(), Rvalue::Use(s), span);
            self.terminate(Terminator::Goto(join));
            self.switch_to(join);
            return Operand::Copy(res);
        }
        self.rvalue_to_temp(Rvalue::ToStr(v), Ty::Str, span)
    }

    /// `` `a${x}b` `` → `"a" + ToStr(x) + "b"`, skipping empty pieces.
    fn template(&mut self, quasis: &[String], parts: &[hir::TemplatePart], span: Span) -> Operand {
        let mut pieces: Vec<Operand> = Vec::new();
        for (i, q) in quasis.iter().enumerate() {
            if !q.is_empty() {
                pieces.push(Operand::Const(Const::Str(q.clone())));
            }
            if let Some(p) = parts.get(i) {
                let v = self.expr(&p.expr);
                let t = self.cx.types.final_ty(p.expr.id).clone();
                match self.cx.types.formats.get(&p.expr.id) {
                    Some(spec) => {
                        let s = self.rvalue_to_temp(Rvalue::Format(v, *spec), Ty::Str, p.expr.span);
                        pieces.push(s);
                    }
                    None => pieces.push(self.stringify(v, &t, p.expr.span)),
                }
            }
        }
        let mut it = pieces.into_iter();
        let Some(mut acc) = it.next() else {
            return Operand::Const(Const::Str(String::new()));
        };
        for p in it {
            acc = self.rvalue_to_temp(Rvalue::Binary(mir::BinOp::Add, acc, p), Ty::Str, span);
        }
        acc
    }

    fn call(&mut self, e: &Expr, callee: &Expr, args: &[Expr], ty: Ty) -> Operand {
        let loc = self.loc(e.span);
        match &callee.kind {
            ExprKind::Member { object, .. } if self.cx.types.methods.contains_key(&e.id) => {
                let m = self.cx.types.methods[&e.id];
                let recv_ty = self.ty_of(object).clone();
                let recv = self.place_of(object);
                let this =
                    self.rvalue_to_temp(Rvalue::Ref(recv), Ty::Ref(Box::new(recv_ty)), e.span);
                let mut ops = vec![this];
                ops.extend(args.iter().map(|a| self.expr(a)));
                let dest = has_value(&ty).then(|| Place::local(self.temp(ty.clone())));
                let next = self.new_block();
                self.terminate(Terminator::Call {
                    callee: Callee::Fn(m),
                    args: ops,
                    dest: dest.clone(),
                    next,
                    loc,
                });
                self.switch_to(next);
                if ty == Ty::Never {
                    self.diverge(Terminator::Unreachable);
                }
                match dest {
                    Some(p) => Operand::Copy(p),
                    None => Operand::Const(Const::Void),
                }
            }
            ExprKind::Member { object, name, .. }
                if matches!(self.ty_of(object), Ty::Array(_) | Ty::Range(_)) =>
            {
                self.builtin_method(e, object, name, args, ty)
            }
            ExprKind::Member { object, name, .. } if name == "toString" => {
                let v = self.expr(object);
                let t = self.cx.types.final_ty(object.id).clone();
                self.stringify(v, &t, e.span)
            }
            ExprKind::Fn(id) => {
                let args: Vec<Operand> = args.iter().map(|a| self.expr(a)).collect();
                let dest = has_value(&ty).then(|| Place::local(self.temp(ty.clone())));
                let next = self.new_block();
                self.terminate(Terminator::Call {
                    callee: Callee::Fn(*id),
                    args,
                    dest: dest.clone(),
                    next,
                    loc,
                });
                self.switch_to(next);
                if ty == Ty::Never {
                    self.diverge(Terminator::Unreachable);
                }
                match dest {
                    Some(p) => Operand::Copy(p),
                    None => Operand::Const(Const::Void),
                }
            }
            ExprKind::Builtin(b @ (Builtin::Print | Builtin::Println)) => {
                let mut ops = Vec::new();
                for a in args {
                    let v = self.expr(a);
                    let t = self.ty_of(a).clone();
                    ops.push(if let Ty::Nullable(_) = t {
                        self.stringify(v, &t, a.span)
                    } else {
                        v
                    });
                }
                let args = ops;
                let next = self.new_block();
                self.terminate(Terminator::Call {
                    callee: Callee::Builtin(*b),
                    args,
                    dest: None,
                    next,
                    loc,
                });
                self.switch_to(next);
                Operand::Const(Const::Void)
            }
            ExprKind::Builtin(Builtin::Assert) => {
                let c = self.expr(&args[0]);
                let ok = self.new_block();
                let fail = self.new_block();
                self.terminate(Terminator::Branch {
                    cond: c,
                    then: ok,
                    else_: fail,
                });
                self.switch_to(fail);
                let msg = match args.get(1) {
                    Some(m) => self.expr(m),
                    None => Operand::Const(Const::Str("assertion failed".into())),
                };
                self.terminate(Terminator::Panic { msg, loc });
                self.switch_to(ok);
                Operand::Const(Const::Void)
            }
            ExprKind::Builtin(Builtin::Panic) => {
                let msg = self.expr(&args[0]);
                self.diverge(Terminator::Panic { msg, loc });
                Operand::Const(Const::Void)
            }
            _ => Operand::Const(Const::Void),
        }
    }
}

/// A literal pattern's value as a constant of the matched type.
fn lit_const(l: &hir::LitPat, ty: &Ty) -> Const {
    match (l, ty) {
        (hir::LitPat::Int(v), Ty::Int(it)) => Const::Int(*v, *it),
        (hir::LitPat::Int(v), Ty::Float(ft)) => Const::Float(*v as f64, *ft),
        (hir::LitPat::Float(v), Ty::Float(ft)) => Const::Float(*v, *ft),
        (hir::LitPat::Str(s), Ty::Char) => Const::Char(s.chars().next().unwrap_or('\0')),
        (hir::LitPat::Str(s), _) => Const::Str(s.clone()),
        (hir::LitPat::Bool(b), _) => Const::Bool(*b),
        _ => unreachable!("type-checked literal pattern"),
    }
}

fn int_const(v: i128, ty: &Ty) -> Operand {
    match ty {
        Ty::Int(it) => Operand::Const(Const::Int(v, *it)),
        Ty::Float(ft) => Operand::Const(Const::Float(v as f64, *ft)),
        _ => Operand::Const(Const::Int(v, IntTy::I64)),
    }
}
