//! Lowering from the typed HIR to MIR.
//!
//! Expressions lower to operands, emitting statements into the current block. Code after a
//! diverging expression (`return`, `break`, `panic`, …) goes into a fresh block with no
//! predecessors, which keeps lowering simple; such blocks end in `unreachable`.

use crate::mir::{
    self, BlockId, Callee, Const, Loc, Local, LocalDecl, Operand, Place, Rvalue, Terminator,
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
    let mir = mir::Program {
        file: file.name.clone(),
        fns,
        entry: program.entry,
        globals,
    };
    (mir, cx.diags)
}

struct Cx<'a> {
    program: &'a hir::Program,
    types: &'a Types,
    file: &'a SourceFile,
    diags: Vec<Diagnostic>,
    reported: HashSet<(u32, u32)>,
}

impl Cx<'_> {
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
        for (l, t) in f.locals.iter().zip(local_tys) {
            b.check_representable(t, l.span);
            b.locals.push(LocalDecl {
                ty: t.clone(),
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
            params: f.params.iter().map(|p| Local(p.0)).collect(),
            ret,
            locals: b.locals,
            blocks,
        }
    }
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

    fn check_representable(&mut self, t: &Ty, span: Span) {
        if matches!(t, Ty::Nullable(_) | Ty::Null) {
            self.cx.not_implemented(span, "nullable values (`T?`)");
        }
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
            Var::Local(l) => Place::Local(Local(l.0)),
            Var::Global(g) => Place::Global(g),
        }
    }

    fn var_ty(&self, v: Var) -> Ty {
        match v {
            Var::Local(l) => self.cx.types.locals[self.fn_id.0 as usize][l.0 as usize].clone(),
            Var::Global(g) => self.cx.types.globals[g.0 as usize].clone(),
        }
    }

    fn rvalue_to_temp(&mut self, rv: Rvalue, ty: Ty, span: Span) -> Operand {
        let t = self.temp(ty);
        self.assign(Place::Local(t), rv, span);
        Operand::Copy(Place::Local(t))
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
        self.check_representable(to, span);
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
            ExprKind::Null => {
                self.cx.not_implemented(e.span, "`null`");
                Operand::Const(Const::Void)
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
            ExprKind::Assign { op, var, value } => {
                let place = self.place(*var);
                match op {
                    None => {
                        let v = self.expr(value);
                        self.assign(place, Rvalue::Use(v), e.span);
                    }
                    Some(op @ (ast::BinOp::And | ast::BinOp::Or)) => {
                        // `x &&= v`: assign only if `x` is true (`||=`: if false).
                        let assign_bb = self.new_block();
                        let join = self.new_block();
                        let cur = Operand::Copy(place);
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
                        self.assign(place, Rvalue::Use(v), e.span);
                        self.terminate(Terminator::Goto(join));
                        self.switch_to(join);
                    }
                    Some(op) => {
                        let v = self.expr(value);
                        self.assign(
                            place,
                            Rvalue::Binary(bin_op(*op), Operand::Copy(place), v),
                            e.span,
                        );
                    }
                }
                Operand::Const(Const::Void)
            }
            ExprKind::IncDec { var, inc, prefix } => {
                let place = self.place(*var);
                let vt = self.var_ty(*var);
                let Ty::Int(it) = vt else {
                    return Operand::Const(Const::Void);
                };
                let old =
                    self.rvalue_to_temp(Rvalue::Use(Operand::Copy(place)), vt.clone(), e.span);
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
            ExprKind::Member { object, name, .. } => {
                debug_assert_eq!(name, "length", "the type checker only allows `.length`");
                let s = self.expr(object);
                self.rvalue_to_temp(Rvalue::StrLen(s), ty, e.span)
            }
            ExprKind::Index { object, index } => {
                let s = self.expr(object);
                let i = self.expr(index);
                self.rvalue_to_temp(Rvalue::StrCharAt(s, i), ty, e.span)
            }
            ExprKind::Template { .. } => {
                self.cx.not_implemented(e.span, "template literals");
                Operand::Const(Const::Void)
            }
            ExprKind::If { cond, then, else_ } => {
                self.if_expr(cond, then, else_.as_deref(), ty, e.span)
            }
            ExprKind::Block(b) => self.block(b),
            ExprKind::Loop(b) => {
                let body = self.new_block();
                let exit = self.new_block();
                let result = has_value(&ty).then(|| Place::Local(self.temp(ty.clone())));
                self.terminate(Terminator::Goto(body));
                self.switch_to(body);
                self.loops.push(LoopTarget {
                    brk: exit,
                    cont: body,
                    result,
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
                let (brk, result) = (target.brk, target.result);
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

    fn binary(&mut self, e: &Expr, op: ast::BinOp, l: &Expr, r: &Expr, ty: Ty) -> Operand {
        match op {
            ast::BinOp::And | ast::BinOp::Or => {
                let res = Place::Local(self.temp(Ty::Bool));
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
                self.assign(res, Rvalue::Use(rv), e.span);
                self.terminate(Terminator::Goto(join));
                self.switch_to(short_bb);
                self.assign(
                    res,
                    Rvalue::Use(Operand::Const(Const::Bool(!is_and))),
                    e.span,
                );
                self.terminate(Terminator::Goto(join));
                self.switch_to(join);
                Operand::Copy(res)
            }
            ast::BinOp::Nullish => Operand::Const(Const::Void),
            _ => {
                let lv = self.expr(l);
                let rv = self.expr(r);
                self.rvalue_to_temp(Rvalue::Binary(bin_op(op), lv, rv), ty, e.span)
            }
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
        // An `if` without `else` has type `T?`, but its value is only representable once it's
        // stored somewhere, and locals are checked when declared.
        let _ = span;
        let result =
            (else_.is_some() && has_value(&ty)).then(|| Place::Local(self.temp(ty.clone())));
        self.terminate(Terminator::Branch {
            cond: c,
            then: then_bb,
            else_: else_bb,
        });
        for (bb, branch) in [(then_bb, Some(then)), (else_bb, else_)] {
            self.switch_to(bb);
            if let Some(branch) = branch {
                let v = self.expr(branch);
                if let Some(r) = result
                    && self.ty(branch) != Ty::Never
                {
                    self.assign(r, Rvalue::Use(v), branch.span);
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

    fn call(&mut self, e: &Expr, callee: &Expr, args: &[Expr], ty: Ty) -> Operand {
        let loc = self.loc(e.span);
        match &callee.kind {
            ExprKind::Fn(id) => {
                let args: Vec<Operand> = args.iter().map(|a| self.expr(a)).collect();
                let dest = has_value(&ty).then(|| Place::Local(self.temp(ty.clone())));
                let next = self.new_block();
                self.terminate(Terminator::Call {
                    callee: Callee::Fn(*id),
                    args,
                    dest,
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
                let args: Vec<Operand> = args.iter().map(|a| self.expr(a)).collect();
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

fn int_const(v: i128, ty: &Ty) -> Operand {
    match ty {
        Ty::Int(it) => Operand::Const(Const::Int(v, *it)),
        Ty::Float(ft) => Operand::Const(Const::Float(v as f64, *ft)),
        _ => Operand::Const(Const::Int(v, IntTy::I64)),
    }
}
