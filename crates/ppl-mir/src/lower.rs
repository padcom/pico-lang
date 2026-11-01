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
use ppl_sema::ty::StructId;
use ppl_sema::{IntTy, Ty, Types};
use ppl_syntax::ast;
use std::collections::{HashMap, HashSet};

pub fn lower(
    program: &hir::Program,
    types: &Types,
    file: &SourceFile,
) -> (mir::Program, Vec<Diagnostic>) {
    let mut cx = Cx {
        program,
        types: std::borrow::Cow::Borrowed(types),
        file,
        diags: Vec::new(),
        reported: HashSet::new(),
        itables: Vec::new(),
        instances: Vec::new(),
        cur_subst: Vec::new(),
        fn_values: Vec::new(),
    };
    // Generic functions get a placeholder; their instances are lowered on demand.
    let mut fns: Vec<mir::Function> = (0..program.fns.len())
        .map(|i| {
            let id = FnId(i as u32);
            // Methods and constructors of generic types are generic too (in the type's
            // parameters), but static initializers aren't.
            if !in_generic_context(program, id) {
                cx.function(id)
            } else {
                mir::Function {
                    name: program.func(id).name.clone(),
                    params: Vec::new(),
                    ret: Ty::Void,
                    locals: Vec::new(),
                    blocks: vec![mir::BasicBlock {
                        stmts: Vec::new(),
                        term: Terminator::Unreachable,
                    }],
                }
            }
        })
        .collect();
    // Monomorphization: each instance is the generic body seen through substituted types.
    let mut next = 0;
    while next < cx.instances.len() {
        let (id, subst) = cx.instances[next].clone();
        let mut t = types.clone();
        for (owner, args) in &subst {
            t = t.substituted(*owner, args);
        }
        cx.types = std::borrow::Cow::Owned(t);
        cx.cur_subst = subst.clone();
        let mut f = cx.function(id);
        // `max<i64>`, `Stack<string>.push`, `Box<i64>.map<string>`.
        let hir = program.func(id);
        let shown = |args: &[Ty]| {
            let parts: Vec<String> = args.iter().map(|t| t.to_string()).collect();
            format!("<{}>", parts.join(", "))
        };
        let own = subst
            .iter()
            .find(|(o, _)| *o == id.0)
            .map(|(_, a)| shown(a));
        let ty = subst.iter().find(|(o, _)| *o != id.0);
        f.name = match (ty, hir.owner) {
            (Some((_, a)), Some(sid)) => {
                let tname = &program.struct_def(sid).name;
                let method = hir.name.rsplit('.').next().unwrap_or(&hir.name);
                format!("{tname}{}.{method}{}", shown(a), own.unwrap_or_default())
            }
            _ => format!("{}{}", hir.name, own.unwrap_or_default()),
        };
        fns.push(f);
        next += 1;
    }
    cx.types = std::borrow::Cow::Borrowed(types);
    cx.cur_subst = Vec::new();
    cx.build_fn_values(&mut fns);
    let itables = cx.build_itables(&mut fns);
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
            is_class: s.is_class,
            meta: mir::ClassMeta {
                base: s.base,
                vtable: s.vtable.clone(),
            },
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
    let mut mir = mir::Program {
        file: file.name.clone(),
        fns,
        entry: program.entry,
        globals,
        structs,
        enums,
        itables,
    };
    erase_program(&mut mir);
    (mir, cx.diags)
}

/// Replaces types the backends don't know (literal types, unions, narrowed variants) with
/// their runtime representation everywhere in the program.
fn erase_program(p: &mut mir::Program) {
    for g in &mut p.globals {
        g.ty = g.ty.erase();
    }
    for s in &mut p.structs {
        for (_, t) in &mut s.fields {
            *t = t.erase();
        }
    }
    for e in &mut p.enums {
        for v in &mut e.variants {
            for (_, t) in &mut v.fields {
                *t = t.erase();
            }
        }
    }
    let erase_op = |o: &mut Operand| {
        if let Operand::Const(Const::Null(t)) = o {
            *t = t.erase();
        }
    };
    for f in &mut p.fns {
        f.ret = f.ret.erase();
        for l in &mut f.locals {
            l.ty = l.ty.erase();
        }
        for b in &mut f.blocks {
            for st in &mut b.stmts {
                match &mut st.rvalue {
                    Rvalue::Tuple(t, ops) | Rvalue::ArrayNew(t, ops) => {
                        *t = t.erase();
                        ops.iter_mut().for_each(erase_op);
                    }
                    Rvalue::WrapSome(o, t) | Rvalue::Cast(o, t) => {
                        *t = t.erase();
                        erase_op(o);
                    }
                    Rvalue::Use(o) => erase_op(o),
                    Rvalue::Variant(_, _, ops) => ops.iter_mut().for_each(erase_op),
                    Rvalue::Aggregate(t, ops) => {
                        *t = t.erase();
                        ops.iter_mut().for_each(erase_op)
                    }
                    Rvalue::New(t) => *t = t.erase(),
                    _ => {}
                }
            }
            match &mut b.term {
                Terminator::Return(Some(o)) => erase_op(o),
                Terminator::Call { args, .. } => args.iter_mut().for_each(erase_op),
                _ => {}
            }
        }
    }
}

/// Whether a function only exists in instances: it's generic, a method or constructor of a
/// generic type, or a closure inside such code.
fn in_generic_context(program: &hir::Program, id: FnId) -> bool {
    let f = program.func(id);
    if !f.generics.is_empty() {
        return true;
    }
    if f.this.is_some()
        && f.owner
            .is_some_and(|s| !program.struct_def(s).generics.is_empty())
    {
        return true;
    }
    match &f.closure {
        Some(c) => in_generic_context(program, c.parent),
        None => false,
    }
}

/// Type arguments for owners of type parameters (`Ty::Param`'s first field).
type Subst = Vec<(u32, Vec<Ty>)>;

struct Cx<'a> {
    program: &'a hir::Program,
    /// The type checker's results; while lowering a generic instance, a copy with its type
    /// arguments substituted.
    types: std::borrow::Cow<'a, Types>,
    /// Generic instances: a function and the substitutions it's seen through (its own type
    /// parameters, and its type's for methods of generic types); instance `i` is MIR
    /// function `program.fns.len() + i`.
    instances: Vec<(FnId, Subst)>,
    /// The substitutions of the instance being lowered (closures inside it share them).
    cur_subst: Subst,
    /// Named functions used as values: (function, the adapter that takes an environment).
    fn_values: Vec<(FnId, FnId)>,
    file: &'a SourceFile,
    diags: Vec<Diagnostic>,
    /// Spans already reported as not implemented (lowering has no such gaps right now).
    #[allow(dead_code)]
    reported: HashSet<(u32, u32)>,
    /// The (implementing type, interface) pairs that need an itable, by itable index.
    itables: Vec<(StructId, StructId)>,
}

impl Cx<'_> {
    /// The MIR function for generic `id` seen through `subst`.
    fn instance(&mut self, id: FnId, subst: Subst) -> FnId {
        let base = self.program.fns.len();
        match self
            .instances
            .iter()
            .position(|(f, a)| *f == id && *a == subst)
        {
            Some(i) => FnId((base + i) as u32),
            None => {
                self.instances.push((id, subst));
                FnId((base + self.instances.len() - 1) as u32)
            }
        }
    }

    /// The adapter that lets named function `id` be called as a function value (it takes
    /// and ignores an environment). Ids are assigned after all instances.
    fn fn_value(&mut self, id: FnId) -> FnId {
        if let Some((_, a)) = self.fn_values.iter().find(|(f, _)| *f == id) {
            return *a;
        }
        // Placeholder id (high range); fixed up in `build_fn_values`.
        let a = FnId(u32::MAX - self.fn_values.len() as u32);
        self.fn_values.push((id, a));
        a
    }

    /// Appends the function-value adapters and patches references to them.
    fn build_fn_values(&mut self, fns: &mut Vec<mir::Function>) {
        let mut map = HashMap::new();
        for (f, placeholder) in self.fn_values.clone() {
            let real = FnId(fns.len() as u32);
            map.insert(placeholder, real);
            let sig = self.types.sigs[f.0 as usize].clone();
            let mut locals = vec![LocalDecl {
                ty: Ty::Ref(Box::new(Ty::Tuple(Vec::new()))),
                name: Some("env".into()),
            }];
            for t in &sig.params {
                locals.push(LocalDecl {
                    ty: t.clone(),
                    name: None,
                });
            }
            let result = has_value(&sig.ret).then(|| {
                locals.push(LocalDecl {
                    ty: sig.ret.clone(),
                    name: None,
                });
                Local(locals.len() as u32 - 1)
            });
            let params: Vec<Local> = (0..=sig.params.len() as u32).map(Local).collect();
            let mut args: Vec<Operand> = params[1..]
                .iter()
                .map(|l| Operand::Copy(Place::local(*l)))
                .collect();
            // Called as a value, every argument is given.
            for d in &self.program.func(f).defaults {
                if d.is_some() {
                    args.push(Operand::Const(Const::Bool(true)));
                }
            }
            fns.push(mir::Function {
                name: format!("{}<value>", self.program.func(f).name),
                params,
                ret: sig.ret.clone(),
                locals,
                blocks: vec![
                    mir::BasicBlock {
                        stmts: Vec::new(),
                        term: Terminator::Call {
                            callee: Callee::Fn(f),
                            args,
                            dest: result.map(Place::local),
                            next: BlockId(1),
                            loc: Loc::default(),
                        },
                    },
                    mir::BasicBlock {
                        stmts: Vec::new(),
                        term: Terminator::Return(result.map(|r| Operand::Copy(Place::local(r)))),
                    },
                ],
            });
        }
        if map.is_empty() {
            return;
        }
        for f in fns.iter_mut() {
            for b in &mut f.blocks {
                for st in &mut b.stmts {
                    if let Rvalue::Closure(id, _) = &mut st.rvalue
                        && let Some(real) = map.get(id)
                    {
                        *id = *real;
                    }
                }
            }
        }
    }

    /// The itable index for `imp` (a struct or class) used as interface `iface`.
    fn itable(&mut self, imp: StructId, iface: StructId) -> u32 {
        match self.itables.iter().position(|p| *p == (imp, iface)) {
            Some(i) => i as u32,
            None => {
                self.itables.push((imp, iface));
                self.itables.len() as u32 - 1
            }
        }
    }

    /// Builds every requested itable. An implementer's method gets an adapter function that
    /// takes the interface value, unpacks the object and calls the method (virtually if it's
    /// virtual); a method the type doesn't define uses the interface's default.
    fn build_itables(&mut self, fns: &mut Vec<mir::Function>) -> Vec<Vec<FnId>> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < self.itables.len() {
            let (imp, iface) = self.itables[i];
            let idef = self.program.struct_def(iface);
            let def = self.program.struct_def(imp);
            let mut entries = Vec::new();
            for (name, ifn) in &idef.methods {
                let entry = match def.method(name) {
                    Some(f) => {
                        let slot = def.virtual_slot(name).filter(|_| def.is_class);
                        let id = FnId(fns.len() as u32);
                        let adapter = self.adapter(imp, iface, *ifn, f, slot, fns);
                        fns.push(adapter);
                        id
                    }
                    None => self.default_method(imp, name).unwrap_or(*ifn),
                };
                entries.push(entry);
            }
            out.push(entries);
            i += 1;
        }
        out
    }

    /// The default implementation of `name` among the interfaces `imp` implements.
    fn default_method(&self, imp: StructId, name: &str) -> Option<FnId> {
        self.program
            .structs
            .iter()
            .enumerate()
            .filter(|(j, d)| d.is_interface && self.program.implements(imp, StructId(*j as u32)))
            .filter_map(|(_, d)| d.method(name).filter(|f| !d.abstract_fns.contains(f)))
            .next()
    }

    /// `fn(self: Iface, args…) { return method(IfaceData(self), args…) }`
    fn adapter(
        &mut self,
        imp: StructId,
        iface: StructId,
        ifn: FnId,
        method: FnId,
        slot: Option<u32>,
        fns: &[mir::Function],
    ) -> mir::Function {
        let sig = &self.types.sigs[ifn.0 as usize];
        let def = self.program.struct_def(imp);
        let data_ty = if def.is_class {
            self.program.struct_ty(imp)
        } else {
            Ty::Ref(Box::new(self.program.struct_ty(imp)))
        };
        let mut locals = vec![LocalDecl {
            ty: self.program.struct_ty(iface),
            name: Some("self".into()),
        }];
        for t in &sig.params {
            locals.push(LocalDecl {
                ty: t.clone(),
                name: None,
            });
        }
        let data = Local(locals.len() as u32);
        locals.push(LocalDecl {
            ty: data_ty,
            name: None,
        });
        let ret = sig.ret.clone();
        let result = has_value(&ret).then(|| {
            locals.push(LocalDecl {
                ty: ret.clone(),
                name: None,
            });
            Local(locals.len() as u32 - 1)
        });
        let params: Vec<Local> = (0..=sig.params.len() as u32).map(Local).collect();
        let mut args = vec![Operand::Copy(Place::local(data))];
        args.extend(params[1..].iter().map(|l| Operand::Copy(Place::local(*l))));
        let callee = match slot {
            Some(slot) => Callee::Virtual { slot, decl: method },
            None => Callee::Fn(method),
        };
        let _ = fns;
        let blocks = vec![
            mir::BasicBlock {
                stmts: vec![mir::Statement {
                    place: Place::local(data),
                    rvalue: Rvalue::IfaceData(Operand::Copy(Place::local(Local(0)))),
                    loc: Loc::default(),
                }],
                term: Terminator::Call {
                    callee,
                    args,
                    dest: result.map(Place::local),
                    next: BlockId(1),
                    loc: Loc::default(),
                },
            },
            mir::BasicBlock {
                stmts: Vec::new(),
                term: Terminator::Return(result.map(|r| Operand::Copy(Place::local(r)))),
            },
        ];
        mir::Function {
            name: format!(
                "{}.{}<{}>",
                def.name,
                self.program
                    .func(method)
                    .name
                    .rsplit('.')
                    .next()
                    .unwrap_or("?"),
                self.program.struct_def(iface).name
            ),
            params,
            ret,
            locals,
            blocks,
        }
    }

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
        let sig = self.types.sigs[id.0 as usize].clone();
        let local_tys = self.types.locals[id.0 as usize].clone();
        let mut b = FnBuilder {
            cx: self,
            hir_fn: f,
            fn_id: id,
            locals: Vec::new(),
            blocks: vec![(Vec::new(), None)],
            cur: BlockId(0),
            loops: Vec::new(),
        };
        for (i, (l, t)) in f.locals.iter().zip(local_tys.iter()).enumerate() {
            // A struct's `this` is passed by reference (a class's is already one).
            let is_this = f.this.is_some_and(|(this, sid)| {
                let d = b.cx.program.struct_def(sid);
                this.0 as usize == i && !d.is_class && !d.is_interface
            });
            // A boxed variable's local holds a reference to its heap cell.
            let ty = if is_this || l.boxed {
                Ty::Ref(Box::new(t.clone()))
            } else {
                t.clone()
            };
            b.locals.push(LocalDecl {
                ty,
                name: Some(l.name.clone()),
            });
        }
        // Parameters arrive by value; a boxed one moves into its cell first.
        let mut params: Vec<Local> = Vec::new();
        let mut env = None;
        if let Some(info) = &f.closure {
            let tys: Vec<Ty> = info
                .captures
                .iter()
                .map(|c| b.locals[c.inner.0 as usize].ty.clone())
                .collect();
            let e = b.temp(Ty::Ref(Box::new(Ty::Tuple(tys))));
            params.push(e);
            env = Some(e);
        } else if let Some((this, _)) = f.this {
            params.push(Local(this.0));
        }
        for p in &f.params {
            if f.local(*p).boxed {
                let t = local_tys[p.0 as usize].clone();
                let incoming = b.temp(t.clone());
                params.push(incoming);
                b.assign(
                    Place::local(Local(p.0)),
                    Rvalue::BoxNew(t, Operand::Copy(Place::local(incoming))),
                    f.span,
                );
            } else {
                params.push(Local(p.0));
            }
        }
        // A flag per defaulted parameter: whether the caller gave it.
        let mut defaulted = Vec::new();
        for (k, d) in f.defaults.iter().enumerate() {
            if d.is_some() {
                let flag = b.temp(Ty::Bool);
                params.push(flag);
                defaulted.push((k, flag));
            }
        }
        if let (Some(e), Some(info)) = (env, &f.closure) {
            // The captures, from the environment.
            for (i, c) in info.captures.iter().enumerate() {
                b.assign(
                    Place::local(Local(c.inner.0)),
                    Rvalue::Use(Operand::Copy(Place::deref(e).field(i as u32))),
                    f.span,
                );
            }
        }
        for (k, flag) in defaulted {
            // `if (!given) param = default`
            let fill = b.new_block();
            let go_on = b.new_block();
            b.terminate(Terminator::Branch {
                cond: Operand::Copy(Place::local(flag)),
                then: go_on,
                else_: fill,
            });
            b.switch_to(fill);
            let d = f.defaults[k].as_ref().expect("defaulted");
            let v = b.expr(d);
            let p = b.place(Var::Local(f.params[k]));
            b.assign(p, Rvalue::Use(v), d.span);
            b.terminate(Terminator::Goto(go_on));
            b.switch_to(go_on);
        }
        let value = b.block(&f.body);
        let ret = if f.is_entry {
            Ty::Void
        } else {
            sig.ret.clone()
        };
        let term = match ret {
            Ty::Void | Ty::Never | Ty::Error => Terminator::Return(None),
            // No value where one is needed: the body diverged, so the end can't be reached.
            _ if value == Operand::Const(Const::Void) => Terminator::Unreachable,
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
            // A method's `this` reference (or a closure's environment) comes first.
            params,
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
            Var::Local(l)
                if self.hir_fn.this.is_some_and(|(this, sid)| {
                    let d = self.cx.program.struct_def(sid);
                    this == l && !d.is_class && !d.is_interface
                }) =>
            {
                Place::deref(Local(l.0))
            }
            Var::Local(l) if self.hir_fn.local(l).boxed => Place::deref(Local(l.0)),
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
            ExprKind::Var(v) if self.cx.types.narrowed.contains(&e.id) => {
                self.narrowed_place(*v, &self.ty(e))
            }
            ExprKind::Var(v) => self.place(*v),
            ExprKind::Member { object, .. }
                if matches!(self.ty_of(object), Ty::Struct(..) | Ty::Class(..))
                    && self.cx.types.fields.contains_key(&e.id) =>
            {
                let i = self.cx.types.fields[&e.id];
                self.place_deferred(object, checks).field(i)
            }
            ExprKind::Member { object, .. } if self.cx.types.variant_fields.contains_key(&e.id) => {
                let (v, i) = self.cx.types.variant_fields[&e.id];
                self.place_deferred(object, checks).variant_field(v, i)
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
                Stmt::Let {
                    var: Var::Local(l),
                    init,
                    span,
                } if self.hir_fn.local(*l).boxed => {
                    // A captured `let`: a new cell for this binding.
                    let t = self.var_ty(Var::Local(*l));
                    let v = match init {
                        Some(init) => self.expr(init),
                        None => Operand::Const(Const::Void),
                    };
                    self.assign(Place::local(Local(l.0)), Rvalue::BoxNew(t, v), *span);
                }
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
        match self.cx.types.coercions.get(&e.id).cloned() {
            Some(to) => {
                let from = self.ty(e);
                self.convert(v, &from, &to, e.span)
            }
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
        // Upcasts between object references (and their nullable forms) keep the pointer, and
        // so do interface upcasts (the parent's methods are a prefix of the child's).
        let class_ref = |t: &Ty| match t {
            Ty::Class(..) => true,
            Ty::Nullable(i) => matches!(**i, Ty::Class(..)),
            _ => false,
        };
        if class_ref(from) && class_ref(to) {
            return v;
        }
        if matches!((from, to), (Ty::Iface(..), Ty::Iface(..))) {
            return v;
        }
        // Literal types are their base type at runtime; a narrowed variant is its enum.
        if from.erase() == to.erase() {
            return v;
        }
        if let Ty::Union(eid, ms) = to {
            let i = ms
                .iter()
                .position(|m| m == from)
                .or_else(|| ms.iter().position(|m| from.widens_to(m)))
                .or_else(|| ms.iter().position(|m| m.erase() == from.erase()))
                .unwrap_or(0);
            let member = ms[i].clone();
            let inner = self.convert(v, from, &member, span);
            return self.rvalue_to_temp(
                Rvalue::Variant(*eid, i as u32, vec![inner]),
                to.clone(),
                span,
            );
        }
        if let (Ty::Class(imp, _, _) | Ty::Struct(imp, _, _), Ty::Iface(iface, _, _)) = (from, to) {
            let it = self.cx.itable(*imp, *iface);
            return self.rvalue_to_temp(Rvalue::ToIface(v, it), to.clone(), span);
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
                Operand::Copy(self.narrowed_place(*v, &ty))
            }
            ExprKind::TypeOf(inner) => {
                let t = self.ty_of(inner).clone();
                let v = self.expr(inner);
                self.type_name(v, &t, e.span)
            }
            ExprKind::Var(v) => Operand::Copy(self.place(*v)),
            ExprKind::Tailcall(inner) => {
                let ExprKind::Call { callee, args, .. } = &inner.kind else {
                    return Operand::Const(Const::Void);
                };
                let id = match &callee.kind {
                    ExprKind::Fn(id) => *id,
                    _ => self.cx.types.overloads[&inner.id],
                };
                let ops = self.call_operands(inner, id, args);
                let loc = self.loc(e.span);
                self.diverge(Terminator::TailCall {
                    callee: id,
                    args: ops,
                    loc,
                });
                Operand::Const(Const::Void)
            }
            ExprKind::Fn(id) if matches!(ty, Ty::Fn(..)) => {
                let a = self.cx.fn_value(*id);
                self.rvalue_to_temp(Rvalue::Closure(a, Vec::new()), ty, e.span)
            }
            ExprKind::Closure(id) => {
                let info = self
                    .cx
                    .program
                    .func(*id)
                    .closure
                    .clone()
                    .expect("a closure");
                let caps: Vec<Operand> = info
                    .captures
                    .iter()
                    .map(|c| {
                        // By reference: the variable's cell; by value: its current value.
                        let l = c.outer;
                        if c.by_ref {
                            Operand::Copy(Place::local(Local(l.0)))
                        } else {
                            Operand::Copy(self.place(Var::Local(l)))
                        }
                    })
                    .collect();
                let fid = if in_generic_context(self.cx.program, *id) {
                    let subst = self.cx.cur_subst.clone();
                    self.cx.instance(*id, subst)
                } else {
                    *id
                };
                self.rvalue_to_temp(Rvalue::Closure(fid, caps), ty, e.span)
            }
            ExprKind::Fn(_) | ExprKind::Builtin(_) | ExprKind::Overloaded(_) | ExprKind::Error => {
                Operand::Const(Const::Void)
            }
            ExprKind::NamedArg { value, .. } | ExprKind::Spread(value) => self.expr(value),
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
            ExprKind::Assign {
                op,
                target: target @ hir::Target::Field { id, object, .. },
                value,
            } if self.cx.types.setters.contains_key(id) => {
                // `obj.prop = v` / `obj.prop += v` through accessors.
                let setter = self.cx.types.setters[id];
                let recv = self.expr(object);
                let v = match op {
                    None => self.expr(value),
                    Some(op) => {
                        let Some(getter) =
                            self.cx.program.struct_def(self.class_of(object)).getter(
                                match target {
                                    hir::Target::Field { name, .. } => name,
                                    _ => unreachable!(),
                                },
                            )
                        else {
                            return Operand::Const(Const::Void);
                        };
                        let vt = self.ty(value);
                        let cur =
                            self.call_fn_value(getter, vec![recv.clone()], vt.clone(), e.span);
                        let rhs = self.expr(value);
                        self.rvalue_to_temp(Rvalue::Binary(bin_op(*op), cur, rhs), vt, e.span)
                    }
                };
                self.call_fn_value(setter, vec![recv, v], Ty::Void, e.span);
                Operand::Const(Const::Void)
            }
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
            ExprKind::Call { callee, args, .. } => self.call(e, callee, args, ty),
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
            ExprKind::Member { object, .. } if self.cx.types.getters.contains_key(&e.id) => {
                let g = self.cx.types.getters[&e.id];
                let recv = self.expr(object);
                self.call_fn_value(g, vec![recv], ty, e.span)
            }
            ExprKind::Member { .. } => {
                let place = self.place_of(e);
                self.read_place(place, e.span)
            }
            ExprKind::CtorCall { args, .. } => {
                let hctor = self.cx.types.ctor_calls[&e.id];
                let ctor = self.method_target(e, hctor);
                let (this, _) = self.hir_fn.this.expect("inside a constructor");
                let mut ops = vec![Operand::Copy(self.place(hir::Var::Local(this)))];
                let rest = self.call_operands(e, hctor, args);
                ops.extend(rest);
                self.call_fn_value(ctor, ops, Ty::Void, e.span)
            }
            ExprKind::SuperCall { method, args } => {
                let (this, _) = self.hir_fn.this.expect("inside a method");
                let mut ops = vec![Operand::Copy(self.place(hir::Var::Local(this)))];
                let rest = self.call_operands(e, *method, args);
                ops.extend(rest);
                self.call_fn_value(*method, ops, ty, e.span)
            }
            ExprKind::InstanceOf { expr, class } => {
                let v = self.expr(expr);
                self.rvalue_to_temp(Rvalue::InstanceOf(v, *class), Ty::Bool, e.span)
            }
            ExprKind::New { args, .. } => {
                let cty = ty.clone();
                let obj = self.rvalue_to_temp(Rvalue::New(cty.clone()), cty, e.span);
                let hctor = self.cx.types.ctor_calls[&e.id];
                let ctor = self.method_target(e, hctor);
                let mut ops = vec![obj.clone()];
                let rest = self.call_operands(e, hctor, args);
                ops.extend(rest);
                self.call_fn_value(ctor, ops, Ty::Void, e.span);
                obj
            }
            ExprKind::Object { fields, .. }
                if self.cx.types.object_variants.contains_key(&e.id) =>
            {
                let (eid, vi, order) = self.cx.types.object_variants[&e.id].clone();
                let n = self.cx.program.enum_def(eid).variants[vi as usize]
                    .fields
                    .len();
                let mut ops: Vec<Option<Operand>> = vec![None; n];
                for (f, slot) in fields.iter().zip(&order) {
                    let v = self.expr(&f.value);
                    ops[*slot as usize] = Some(v);
                }
                // Optional fields left out are null.
                let tys: Vec<Ty> = self.cx.program.enum_def(eid).variants[vi as usize]
                    .fields
                    .iter()
                    .map(|f| f.ty.clone())
                    .collect();
                let ops = ops
                    .into_iter()
                    .zip(tys)
                    .map(|(o, t)| o.unwrap_or(Operand::Const(Const::Null(t))))
                    .collect();
                self.rvalue_to_temp(Rvalue::Variant(eid, vi, ops), ty, e.span)
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
                let _ = sid;
                self.rvalue_to_temp(Rvalue::Aggregate(ty.clone(), ops), ty, e.span)
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
                rebox,
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
                    // Closures of this iteration keep its cell; the next one gets a copy.
                    for l in rebox {
                        let t = self.var_ty(Var::Local(*l));
                        self.assign(
                            Place::local(Local(l.0)),
                            Rvalue::BoxNew(t, Operand::Copy(Place::deref(Local(l.0)))),
                            e.span,
                        );
                    }
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
                    // After a diverging value (`tailcall`, `panic`), the return is dead.
                    Some((Operand::Const(Const::Void), _)) if ret_has_value => {
                        Terminator::Unreachable
                    }
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
        if let Some(&i) = self.cx.types.union_tests.get(&p.id) {
            // Which member the union holds, then the pattern on that member.
            let tag = self.rvalue_to_temp(
                Rvalue::Tag(Operand::Copy(place.clone())),
                Ty::Int(IntTy::U32),
                p.span,
            );
            let eq = self.rvalue_to_temp(
                Rvalue::Binary(
                    mir::BinOp::Eq,
                    tag,
                    Operand::Const(Const::Int(i as i128, IntTy::U32)),
                ),
                Ty::Bool,
                p.span,
            );
            self.branch_unless(eq, fail);
            return self.match_pattern_inner(p, &place.variant_field(i, 0), fail);
        }
        self.match_pattern_inner(p, place, fail)
    }

    fn match_pattern_inner(&mut self, p: &hir::Pattern, place: &Place, fail: BlockId) {
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
            return self.match_pattern_inner(p, &place.payload(), fail);
        }
        match &p.kind {
            PatKind::Wild => {}
            PatKind::Bind(l) => self.bind(*l, place, p.span),
            PatKind::As(l, sub) => {
                self.bind(*l, place, p.span);
                self.match_pattern(sub, place, fail);
            }
            PatKind::Type {
                ty: want,
                bind,
                fields,
            } => {
                let place = &self.downcast(p, want, place, fail);
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
            PatKind::Struct { sid, fields } => {
                let st = self.cx.program.struct_ty(*sid);
                let place = &self.downcast(p, &st, place, fail);
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

    /// For a downcast pattern: checks the runtime class (failing otherwise) and returns the
    /// object in a temporary of the subclass type. Other patterns keep their place.
    fn downcast(&mut self, p: &hir::Pattern, to: &Ty, place: &Place, fail: BlockId) -> Place {
        if !self.cx.types.downcasts.contains(&p.id) {
            return place.clone();
        }
        let Ty::Class(sid, _, _) = to else {
            return place.clone();
        };
        let ok = self.rvalue_to_temp(
            Rvalue::InstanceOf(Operand::Copy(place.clone()), *sid),
            Ty::Bool,
            p.span,
        );
        self.branch_unless(ok, fail);
        let t = Place::local(self.temp(to.clone()));
        self.assign(t.clone(), Rvalue::Use(Operand::Copy(place.clone())), p.span);
        t
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
        if let Ty::Union(_, ms) = t {
            // The string form of the member it holds.
            let res = Place::local(self.temp(Ty::Str));
            let tag = self.rvalue_to_temp(Rvalue::Tag(v.clone()), Ty::Int(IntTy::U32), span);
            let join = self.new_block();
            let Operand::Copy(vp) = v else {
                return Operand::Const(Const::Str(String::new()));
            };
            for (i, m) in ms.iter().enumerate() {
                let yes = self.new_block();
                let no = self.new_block();
                let eq = self.rvalue_to_temp(
                    Rvalue::Binary(
                        mir::BinOp::Eq,
                        tag.clone(),
                        Operand::Const(Const::Int(i as i128, IntTy::U32)),
                    ),
                    Ty::Bool,
                    span,
                );
                self.terminate(Terminator::Branch {
                    cond: eq,
                    then: yes,
                    else_: no,
                });
                self.switch_to(yes);
                let part = Operand::Copy(vp.variant_field(i as u32, 0));
                let s = self.stringify(part, &m.erase(), span);
                self.assign(res.clone(), Rvalue::Use(s), span);
                self.terminate(Terminator::Goto(join));
                self.switch_to(no);
            }
            self.terminate(Terminator::Goto(join));
            self.switch_to(join);
            return Operand::Copy(res);
        }
        if let Ty::Lits(b, _) = t {
            return self.stringify(v, b, span);
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

    /// Where a narrowed variable's value is: inside the `T?`, or inside the union member.
    fn narrowed_place(&self, v: Var, narrowed: &Ty) -> Place {
        let mut p = self.place(v);
        let mut declared = self.var_ty(v);
        if let Ty::Nullable(inner) = declared {
            p = p.payload();
            declared = *inner;
        }
        if let Ty::Union(_, ms) = &declared
            && let Some(i) = ms.iter().position(|m| m == narrowed)
        {
            p = p.variant_field(i as u32, 0);
        }
        p
    }

    /// `typeof v`: the type's name; for a union, the name of the member it holds.
    fn type_name(&mut self, v: Operand, t: &Ty, span: Span) -> Operand {
        let Ty::Union(_, ms) = t else {
            let name = match t {
                Ty::Nullable(_) => {
                    // "null", or the inner type's name.
                    let res = Place::local(self.temp(Ty::Str));
                    let is_null = self.rvalue_to_temp(Rvalue::IsNull(v), Ty::Bool, span);
                    let (a, b, join) = (self.new_block(), self.new_block(), self.new_block());
                    self.terminate(Terminator::Branch {
                        cond: is_null,
                        then: a,
                        else_: b,
                    });
                    let inner = match t {
                        Ty::Nullable(i) => i.erase().to_string(),
                        _ => unreachable!(),
                    };
                    for (bb, s) in [(a, "null".to_string()), (b, inner)] {
                        self.switch_to(bb);
                        self.assign(
                            res.clone(),
                            Rvalue::Use(Operand::Const(Const::Str(s))),
                            span,
                        );
                        self.terminate(Terminator::Goto(join));
                    }
                    self.switch_to(join);
                    return Operand::Copy(res);
                }
                t => t.erase().to_string(),
            };
            return Operand::Const(Const::Str(name));
        };
        let names: Vec<String> = ms.iter().map(|m| m.erase().to_string()).collect();
        self.by_tag(v, &names, span)
    }

    /// One string per union member, picked by the value's tag.
    fn by_tag(&mut self, v: Operand, texts: &[String], span: Span) -> Operand {
        let res = Place::local(self.temp(Ty::Str));
        let tag = self.rvalue_to_temp(Rvalue::Tag(v), Ty::Int(IntTy::U32), span);
        let join = self.new_block();
        for (i, text) in texts.iter().enumerate() {
            let yes = self.new_block();
            let no = self.new_block();
            let eq = self.rvalue_to_temp(
                Rvalue::Binary(
                    mir::BinOp::Eq,
                    tag.clone(),
                    Operand::Const(Const::Int(i as i128, IntTy::U32)),
                ),
                Ty::Bool,
                span,
            );
            self.terminate(Terminator::Branch {
                cond: eq,
                then: yes,
                else_: no,
            });
            self.switch_to(yes);
            self.assign(
                res.clone(),
                Rvalue::Use(Operand::Const(Const::Str(text.clone()))),
                span,
            );
            self.terminate(Terminator::Goto(join));
            self.switch_to(no);
        }
        self.terminate(Terminator::Goto(join));
        self.switch_to(join);
        Operand::Copy(res)
    }

    /// A call's argument operands for HIR function `f` (in parameter order, then a flag per
    /// defaulted parameter saying whether it was given).
    fn call_operands(&mut self, e: &Expr, f: FnId, args: &[Expr]) -> Vec<Operand> {
        fn value(a: &Expr) -> &Expr {
            match &a.kind {
                ExprKind::NamedArg { value, .. } | ExprKind::Spread(value) => value,
                _ => a,
            }
        }
        let Some(slots) = self.cx.types.arg_slots.get(&e.id).cloned() else {
            return args.iter().map(|a| self.expr(value(a))).collect();
        };
        let sig = self.cx.types.sigs[f.0 as usize].clone();
        let hf = self.cx.program.func(f);
        let mut tuples: HashMap<usize, Place> = HashMap::new();
        let mut ops = Vec::new();
        let mut flags = Vec::new();
        for (k, slot) in slots.iter().enumerate() {
            let pt = sig.params[k].clone();
            let op = match slot {
                ppl_sema::typeck::Slot::Arg(i) => self.expr(value(&args[*i])),
                ppl_sema::typeck::Slot::Default => Operand::Const(Const::Undef(pt.clone())),
                ppl_sema::typeck::Slot::Rest(is) => {
                    let elem = match &pt {
                        Ty::Array(t) => (**t).clone(),
                        _ => Ty::Error,
                    };
                    let vals: Vec<Operand> = is.iter().map(|i| self.expr(&args[*i])).collect();
                    self.rvalue_to_temp(Rvalue::ArrayNew(elem, vals), pt.clone(), e.span)
                }
                ppl_sema::typeck::Slot::RestSpread(i) => self.expr(value(&args[*i])),
                ppl_sema::typeck::Slot::TupleElem(i, j) => {
                    let place = match tuples.get(i) {
                        Some(p) => p.clone(),
                        None => {
                            let p = self.place_of(value(&args[*i]));
                            tuples.insert(*i, p.clone());
                            p
                        }
                    };
                    let v = Operand::Copy(place.field(*j));
                    let et = self.place_ty(&place.field(*j));
                    self.convert(v, &et, &pt, e.span)
                }
            };
            ops.push(op);
            if hf.defaults.get(k).is_some_and(Option::is_some) {
                let given = !matches!(slot, ppl_sema::typeck::Slot::Default);
                flags.push(Operand::Const(Const::Bool(given)));
            }
        }
        ops.extend(flags);
        ops
    }

    /// The function a method or constructor call runs: an instance when the type (or the
    /// method) is generic.
    fn method_target(&mut self, e: &Expr, m: FnId) -> FnId {
        let mut subst = Subst::new();
        if let Some(args) = self.cx.types.recv_args.get(&e.id).cloned()
            && let Some(sid) = self.cx.program.func(m).owner
            && !args.is_empty()
        {
            subst.push((Ty::type_owner(sid), args));
        }
        if let Some(args) = self.cx.types.instances.get(&e.id).cloned()
            && !args.is_empty()
        {
            subst.push((m.0, args));
        }
        if subst.is_empty() {
            m
        } else {
            self.cx.instance(m, subst)
        }
    }

    /// The class an expression's (class-typed) value belongs to.
    fn class_of(&self, e: &Expr) -> ppl_sema::ty::StructId {
        match self.ty_of(e) {
            Ty::Class(sid, _, _) | Ty::Struct(sid, _, _) => *sid,
            t => unreachable!("not a class: {t}"),
        }
    }

    /// A method call on a type parameter's value, now that the instance's type is known:
    /// `compareTo`/`equals` on primitives become comparisons; otherwise the concrete type's
    /// method (through its vtable or itable when needed).
    fn param_call(
        &mut self,
        e: &Expr,
        object: &Expr,
        name: &str,
        args: &[Expr],
        ty: Ty,
    ) -> Operand {
        let t = self.ty_of(object).clone();
        let primitive = matches!(
            t.erase(),
            Ty::Int(_) | Ty::Float(_) | Ty::Char | Ty::Str | Ty::Bool
        );
        if primitive && (name == "compareTo" || name == "equals") {
            let a = self.expr(object);
            let b = self.expr(&args[0]);
            if name == "equals" {
                return self.rvalue_to_temp(Rvalue::Binary(mir::BinOp::Eq, a, b), Ty::Bool, e.span);
            }
            // -1, 0 or 1.
            let res = Place::local(self.temp(Ty::Int(IntTy::I32)));
            let lt = self.rvalue_to_temp(
                Rvalue::Binary(mir::BinOp::Lt, a.clone(), b.clone()),
                Ty::Bool,
                e.span,
            );
            let gt = self.rvalue_to_temp(Rvalue::Binary(mir::BinOp::Gt, a, b), Ty::Bool, e.span);
            let (neg, not_neg, pos, zero, join) = (
                self.new_block(),
                self.new_block(),
                self.new_block(),
                self.new_block(),
                self.new_block(),
            );
            self.terminate(Terminator::Branch {
                cond: lt,
                then: neg,
                else_: not_neg,
            });
            self.switch_to(not_neg);
            self.terminate(Terminator::Branch {
                cond: gt,
                then: pos,
                else_: zero,
            });
            for (bb, v) in [(neg, -1), (pos, 1), (zero, 0)] {
                self.switch_to(bb);
                self.assign(
                    res.clone(),
                    Rvalue::Use(Operand::Const(Const::Int(v, IntTy::I32))),
                    e.span,
                );
                self.terminate(Terminator::Goto(join));
            }
            self.switch_to(join);
            return Operand::Copy(res);
        }
        let sid = match &t {
            Ty::Struct(s, _, _) | Ty::Class(s, _, _) | Ty::Iface(s, _, _) => *s,
            other => unreachable!("method `{name}` on {other}"),
        };
        let def = self.cx.program.struct_def(sid);
        let m = def.method(name).expect("bound guarantees the method");
        let loc = self.loc(e.span);
        let (callee, recv) = match &t {
            Ty::Iface(..) => {
                let slot = def.methods.iter().position(|(n, _)| n == name).unwrap_or(0) as u32;
                (Callee::Iface { slot, decl: m }, self.expr(object))
            }
            Ty::Class(..) => {
                let callee = match def.virtual_slot(name) {
                    Some(slot) => Callee::Virtual { slot, decl: m },
                    None => Callee::Fn(m),
                };
                (callee, self.expr(object))
            }
            _ => {
                let place = self.place_of(object);
                let r =
                    self.rvalue_to_temp(Rvalue::Ref(place), Ty::Ref(Box::new(t.clone())), e.span);
                (Callee::Fn(m), r)
            }
        };
        let mut ops = vec![recv];
        ops.extend(args.iter().map(|a| self.expr(a)));
        let dest = has_value(&ty).then(|| Place::local(self.temp(ty.clone())));
        let next = self.new_block();
        self.terminate(Terminator::Call {
            callee,
            args: ops,
            dest: dest.clone(),
            next,
            loc,
        });
        self.switch_to(next);
        match dest {
            Some(p) => Operand::Copy(p),
            None => Operand::Const(Const::Void),
        }
    }

    /// Calls function `id` with already-evaluated arguments.
    fn call_fn_value(&mut self, id: FnId, args: Vec<Operand>, ty: Ty, span: Span) -> Operand {
        let loc = self.loc(span);
        let dest = has_value(&ty).then(|| Place::local(self.temp(ty.clone())));
        let next = self.new_block();
        self.terminate(Terminator::Call {
            callee: Callee::Fn(id),
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

    fn call(&mut self, e: &Expr, callee: &Expr, args: &[Expr], ty: Ty) -> Operand {
        let loc = self.loc(e.span);
        match &callee.kind {
            ExprKind::Member { object, name, .. }
                if self.cx.types.param_calls.contains_key(&e.id) =>
            {
                self.param_call(e, object, name, args, ty)
            }
            ExprKind::Member { object, .. } if self.cx.types.iface_calls.contains_key(&e.id) => {
                let slot = self.cx.types.iface_calls[&e.id];
                let decl = self
                    .cx
                    .types
                    .methods
                    .get(&e.id)
                    .copied()
                    .unwrap_or_else(|| {
                        let Ty::Iface(sid, _, _) = self.ty_of(object) else {
                            unreachable!("interface call")
                        };
                        self.cx.program.struct_def(*sid).methods[slot as usize].1
                    });
                let mut ops = vec![self.expr(object)];
                ops.extend(args.iter().map(|a| self.expr(a)));
                let loc = self.loc(e.span);
                let dest = has_value(&ty).then(|| Place::local(self.temp(ty.clone())));
                let next = self.new_block();
                self.terminate(Terminator::Call {
                    callee: Callee::Iface { slot, decl },
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
            ExprKind::Member { object, .. }
                if self.cx.types.methods.contains_key(&e.id)
                    && matches!(self.ty_of(object), Ty::Class(..)) =>
            {
                // A class method takes the object reference itself.
                let hm = self.cx.types.methods[&e.id];
                let m = self.method_target(e, hm);
                let mut ops = vec![self.expr(object)];
                let rest = self.call_operands(e, hm, args);
                ops.extend(rest);
                match self.cx.types.virtual_calls.get(&e.id) {
                    Some(&slot) => {
                        let loc = self.loc(e.span);
                        let dest = has_value(&ty).then(|| Place::local(self.temp(ty.clone())));
                        let next = self.new_block();
                        self.terminate(Terminator::Call {
                            callee: Callee::Virtual { slot, decl: m },
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
                    None => self.call_fn_value(m, ops, ty, e.span),
                }
            }
            ExprKind::Member { object, .. } if self.cx.types.methods.contains_key(&e.id) => {
                let hm = self.cx.types.methods[&e.id];
                let m = self.method_target(e, hm);
                let recv_ty = self.ty_of(object).clone();
                let recv = self.place_of(object);
                let this =
                    self.rvalue_to_temp(Rvalue::Ref(recv), Ty::Ref(Box::new(recv_ty)), e.span);
                let mut ops = vec![this];
                let rest = self.call_operands(e, hm, args);
                ops.extend(rest);
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
            ExprKind::Fn(_) | ExprKind::Overloaded(_) => {
                let id = &match &callee.kind {
                    ExprKind::Fn(id) => *id,
                    _ => self.cx.types.overloads[&e.id],
                };
                let args: Vec<Operand> = self.call_operands(e, *id, args);
                let id = match self.cx.types.instances.get(&e.id).cloned() {
                    Some(targs) => self.cx.instance(*id, vec![(id.0, targs)]),
                    None => *id,
                };
                let dest = has_value(&ty).then(|| Place::local(self.temp(ty.clone())));
                let next = self.new_block();
                self.terminate(Terminator::Call {
                    callee: Callee::Fn(id),
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
            _ if matches!(self.ty_of(callee), Ty::Fn(..)) => {
                let f = self.expr(callee);
                let mut ops = vec![f];
                ops.extend(args.iter().map(|a| self.expr(a)));
                let dest = has_value(&ty).then(|| Place::local(self.temp(ty.clone())));
                let next = self.new_block();
                self.terminate(Terminator::Call {
                    callee: Callee::Indirect,
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
