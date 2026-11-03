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
use ppl_sema::ty::{LitVal, StructId};
use ppl_sema::{IntTy, Ty, Types};
use ppl_syntax::ast;
use std::collections::{HashMap, HashSet};

pub fn lower(
    program: &hir::Program,
    types: &Types,
    file: &SourceFile,
    file_id: ppl_diag::FileId,
) -> (mir::Program, Vec<Diagnostic>) {
    let mut cx = Cx {
        program,
        types: TypesRef::Borrowed(types),
        file,
        file_id,
        diags: Vec::new(),
        reported: HashSet::new(),
        itables: Vec::new(),
        instances: Vec::new(),
        cur_subst: Vec::new(),
        fn_values: Vec::new(),
        cur_mir: FnId(0),
        gen_frames: HashMap::new(),
        skip_files: program.workers.iter().copied().collect(),
        inline_entry: false,
        synths: Vec::new(),
    };
    // Generic functions get a placeholder; their instances are lowered on demand.
    let mut fns: Vec<mir::Function> = (0..program.fns.len())
        .map(|i| {
            let id = FnId(i as u32);
            // Methods and constructors of generic types are generic too (in the type's
            // parameters), but static initializers aren't.
            if !in_generic_context(program, id) {
                cx.cur_mir = id;
                cx.function(id)
            } else {
                mir::Function {
                    name: program.func(id).name.clone(),
                    library: true,
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
    // Each worker's thread starts in a copy of the entry's code: the library modules' parts,
    // then its own module's (spec §10.2).
    let mut worker_fns = Vec::new();
    for (k, w) in program.workers.iter().enumerate() {
        cx.skip_files = program
            .workers
            .iter()
            .copied()
            .filter(|f| f != w)
            .chain([file_id])
            .collect();
        cx.cur_mir = FnId(u32::MAX - k as u32);
        let mut f = cx.function(program.entry);
        f.name = format!("<worker {}>", k + 1);
        worker_fns.push(f);
    }
    // Inline workers (`new Worker(f)`) start in the library modules' code, then wait for the
    // function to serve.
    if program.uses_inline_workers {
        cx.skip_files = program.workers.iter().copied().chain([file_id]).collect();
        cx.inline_entry = true;
        cx.cur_mir = FnId(u32::MAX - program.workers.len() as u32);
        let mut f = cx.function(program.entry);
        f.name = "<inline worker>".into();
        cx.inline_entry = false;
        worker_fns.push(f);
    }
    cx.skip_files.clear();
    // Monomorphization: each instance is the generic body seen through substituted types.
    // Show functions may need more instances and the other way round.
    let mut next = 0;
    let mut substituted: Vec<(Subst, std::rc::Rc<Types>)> = Vec::new();
    let mut synth_fns: Vec<mir::Function> = Vec::new();
    while next < cx.instances.len() || synth_fns.len() < cx.synths.len() {
        if next == cx.instances.len() {
            cx.types = TypesRef::Borrowed(types);
            cx.cur_subst = Vec::new();
            let (what, id) = cx.synths[synth_fns.len()].clone();
            synth_fns.push(cx.synth_function(&what, id));
            continue;
        }
        let (id, subst) = cx.instances[next].clone();
        // Instances with the same substitution (the methods of `Map<string, i64>`) share the
        // substituted tables: building them is most of lowering's time otherwise.
        let shared = match substituted.iter().find(|(s, _)| *s == subst) {
            Some((_, t)) => t.clone(),
            None => {
                let mut t = types.clone();
                for (owner, args) in &subst {
                    t = t.substituted(*owner, args);
                }
                let t = std::rc::Rc::new(t);
                substituted.push((subst.clone(), t.clone()));
                t
            }
        };
        cx.types = TypesRef::Shared(shared);
        cx.cur_subst = subst.clone();
        cx.cur_mir = FnId((program.fns.len() + next) as u32);
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
    cx.types = TypesRef::Borrowed(types);
    cx.cur_subst = Vec::new();
    let mut worker_entries: Vec<FnId> = (0..worker_fns.len())
        .map(|i| FnId((fns.len() + i) as u32))
        .collect();
    let inline_worker_entry = program
        .uses_inline_workers
        .then(|| worker_entries.pop().expect("pushed"));
    fns.extend(worker_fns);
    // The synthesized functions come last; calls of them get their real ids.
    if !synth_fns.is_empty() {
        let first = fns.len() as u32;
        fns.extend(synth_fns);
        for f in &mut fns {
            for b in &mut f.blocks {
                if let Terminator::Call {
                    callee: Callee::Fn(id),
                    ..
                } = &mut b.term
                    && id.0 >= SYNTH_BASE
                    && id.0 < SYNTH_BASE + cx.synths.len() as u32
                {
                    *id = FnId(first + id.0 - SYNTH_BASE);
                }
            }
        }
    }
    // A generator's environment also holds its frame: start in state 0, locals unset.
    for f in &mut fns {
        for b in &mut f.blocks {
            for st in &mut b.stmts {
                if let Rvalue::Closure(id, ops) = &mut st.rvalue
                    && let Some(slots) = cx.gen_frames.get(id)
                {
                    ops.push(Operand::Const(Const::Int(0, IntTy::I64)));
                    ops.extend(
                        slots[1..]
                            .iter()
                            .map(|t| Operand::Const(Const::Undef(t.clone()))),
                    );
                }
            }
        }
    }
    cx.build_fn_values(&mut fns);
    let itables = cx.build_itables(&mut fns);
    let globals = program
        .globals
        .iter()
        .zip(&types.globals)
        .map(|(g, t)| mir::GlobalDecl {
            name: g.name.clone(),
            ty: t.clone(),
            span: Some(g.span),
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
            weak: s.fields.iter().map(|f| f.weak).collect(),
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
    let exports = export_wrappers(&mut cx, &mut fns);
    check_externs(&mut cx);
    let mut mir = mir::Program {
        exports,
        externs: program
            .externs
            .iter()
            .map(|e| (e.func, e.symbol.clone()))
            .collect(),
        links: program.links.clone(),
        file: file.name.clone(),
        fns,
        entry: program.entry,
        worker_entries,
        inline_worker_entry,
        globals,
        structs,
        enums,
        itables,
        sources: Vec::new(),
    };
    erase_program(&mut mir);
    drop_unused_exit_hook(&mut mir);
    cx.diags.extend(crate::fold::fold_program(&mut mir));
    (mir, cx.diags)
}

/// Why `t` can't cross to JavaScript (spec §12.3): numbers, `bool`, `string`, structs of
/// those (copied as plain objects) and classes (handles) can.
fn js_problem(p: &hir::Program, t: &Ty, seen: &mut Vec<StructId>) -> Option<String> {
    match t {
        Ty::Int(_) | Ty::Float(_) | Ty::Bool | Ty::Str => None,
        Ty::Class(_, _, args) if args.is_empty() => None,
        Ty::Struct(sid, name, args) if args.is_empty() => {
            if seen.contains(sid) {
                return None;
            }
            seen.push(*sid);
            p.struct_def(*sid).fields.iter().find_map(|f| {
                js_problem(p, &f.ty, seen).map(|why| format!("field `{name}.{}`: {why}", f.name))
            })
        }
        _ => Some(format!(
            "`{t}` can't cross to JavaScript (numbers, `bool`, `string`, structs of those and classes can)"
        )),
    }
}

/// What an `extern "C"` function may take and return (spec §12.1): numbers, `bool`, pointers
/// (callbacks too) and, as parameters, `string` (a temporary C string). The core library's
/// externs may return a ppl `string` from the runtime.
fn check_externs(cx: &mut Cx) {
    let p = cx.program;
    for e in &p.externs {
        let f = p.func(e.func);
        let sig = &cx.types.sigs[e.func.0 as usize];
        let core = f.span.file != cx.file_id && cx.program.func(e.func).name.starts_with("ppl_");
        let ok = |t: &Ty, ret: bool| match t {
            Ty::Int(_) | Ty::Float(_) | Ty::Bool | Ty::Ref(_) => true,
            Ty::Str => !ret || core,
            Ty::Void => ret,
            _ => false,
        };
        let bad = sig
            .params
            .iter()
            .find(|t| !ok(t, false))
            .or_else(|| (!ok(&sig.ret, true)).then_some(&sig.ret));
        if let Some(t) = bad {
            let hint = if *t == Ty::Str {
                "return `ptr<u8>` and copy it with `string.fromC(p)`"
            } else {
                "C takes numbers, `bool`, pointers and strings (structs by pointer for now)"
            };
            cx.diags.push(
                Diagnostic::error(
                    codes::EXPORT_TYPE,
                    format!("`{}` can't have `{t}` in a C signature", f.name),
                )
                .primary(f.name_span, "declared here")
                .help(hint),
            );
        }
    }
}

/// The wrappers of `@export` functions (decision 258): `(out?, params…)`, where struct
/// parameters come as pointers to a copy and a struct result is stored through `out`.
fn export_wrappers(cx: &mut Cx, fns: &mut Vec<mir::Function>) -> Vec<mir::Export> {
    let p = cx.program;
    let mut out = Vec::new();
    for e in &p.exports {
        let f = p.func(e.func);
        let sig = cx.types.sigs[e.func.0 as usize].clone();
        let mut bad = Vec::new();
        if !f.throws.is_empty() {
            bad.push("it throws: an exported function has to catch its errors".to_string());
        }
        if e.abi == hir::ExportAbi::C {
            // C (spec §12.4): numbers, `bool` and pointers (structs by pointer for now).
            for t in sig.params.iter().chain(std::iter::once(&sig.ret)) {
                let ok = matches!(t, Ty::Int(_) | Ty::Float(_) | Ty::Bool | Ty::Ref(_))
                    || (*t == Ty::Void && std::ptr::eq(t, &sig.ret));
                if !ok {
                    bad.push(format!(
                        "`{t}` can't cross to C (numbers, `bool` and pointers can)"
                    ));
                }
            }
        }
        if e.abi == hir::ExportAbi::Js {
            for t in sig
                .params
                .iter()
                .chain(std::iter::once(&sig.ret).filter(|t| !matches!(t, Ty::Void)))
            {
                bad.extend(js_problem(p, t, &mut Vec::new()));
            }
        }
        if f.rest || f.defaults.iter().any(Option::is_some) {
            bad.push("default and rest parameters can't be exported yet".into());
        }
        if let Some(why) = bad.first() {
            cx.diags.push(
                Diagnostic::error(
                    codes::EXPORT_TYPE,
                    format!("`{}` can't be exported: {why}", e.name),
                )
                .primary(e.span, "exported here"),
            );
            continue;
        }
        let js = e.abi == hir::ExportAbi::Js;
        let by_ptr = |t: &Ty| js && matches!(t, Ty::Struct(..));
        let mut locals = Vec::new();
        let out_param = by_ptr(&sig.ret).then(|| {
            locals.push(LocalDecl {
                ty: Ty::Ref(Box::new(sig.ret.clone())),
                name: Some("out".into()),
            });
            Local(0)
        });
        let mut args = Vec::new();
        for (i, t) in sig.params.iter().enumerate() {
            let l = Local(locals.len() as u32);
            let name = f
                .params
                .get(i)
                .map(|pl| f.local(*pl).name.clone())
                .unwrap_or_default();
            if by_ptr(t) {
                locals.push(LocalDecl {
                    ty: Ty::Ref(Box::new(t.clone())),
                    name: Some(name),
                });
                args.push(Operand::Copy(Place::deref(l)));
            } else {
                locals.push(LocalDecl {
                    ty: t.clone(),
                    name: Some(name),
                });
                args.push(Operand::Copy(Place::local(l)));
            }
        }
        let params: Vec<Local> = (0..locals.len() as u32).map(Local).collect();
        let result = has_value(&sig.ret).then(|| {
            locals.push(LocalDecl {
                ty: sig.ret.clone(),
                name: None,
            });
            Local(locals.len() as u32 - 1)
        });
        let (ret, last) = match (out_param, result) {
            (Some(o), Some(r)) => (
                Ty::Void,
                mir::BasicBlock {
                    stmts: vec![mir::Statement {
                        place: Place::deref(o),
                        rvalue: Rvalue::Use(Operand::Copy(Place::local(r))),
                        loc: Loc::default(),
                    }],
                    term: Terminator::Return(None),
                },
            ),
            _ => (
                sig.ret.clone(),
                mir::BasicBlock {
                    stmts: Vec::new(),
                    term: Terminator::Return(result.map(|r| Operand::Copy(Place::local(r)))),
                },
            ),
        };
        let blocks = vec![
            mir::BasicBlock {
                stmts: Vec::new(),
                term: Terminator::Call {
                    callee: Callee::Fn(e.func),
                    args,
                    dest: result.map(Place::local),
                    next: BlockId(1),
                    loc: Loc::default(),
                },
            },
            last,
        ];
        let wrapper = FnId(fns.len() as u32);
        fns.push(mir::Function {
            library: false,
            name: format!("{}<export>", e.name),
            params,
            ret,
            locals,
            blocks,
        });
        out.push(mir::Export {
            wrapper,
            func: e.func,
            name: e.name.clone(),
            abi: e.abi,
            params: f
                .params
                .iter()
                .zip(&sig.params)
                .map(|(l, t)| (f.local(*l).name.clone(), t.clone()))
                .collect(),
            ret: sig.ret.clone(),
        });
    }
    out
}

/// Without timers, microtasks or a `FinalizationRegistry` there's no loop to run at exit: the
/// entry's call of `__runEventLoop` becomes a jump (so it and what it uses aren't compiled).
fn drop_unused_exit_hook(p: &mut mir::Program) {
    let Some(hook) = p
        .fns
        .iter()
        .position(|f| f.library && f.name == "__runEventLoop")
        .map(|i| FnId(i as u32))
    else {
        return;
    };
    // (Generic classes' functions are instantiated per type: any live one of the class's.)
    let live = mir::live_functions(p);
    let used = p.fns.iter().zip(&live).any(|(f, l)| {
        *l && f.library
            && [
                "FinalizationRegistry",
                "setTimeout",
                "setInterval",
                "queueMicrotask",
                "Worker",
                "__workerOnMessage",
                "__ioSubmit",
            ]
            .iter()
            .any(|n| f.name.starts_with(n))
    });
    if used {
        return;
    }
    for b in &mut p.fns[p.entry.0 as usize].blocks {
        if let Terminator::Call {
            callee: Callee::Fn(f),
            next,
            ..
        } = &b.term
            && *f == hook
        {
            b.term = Terminator::Goto(*next);
        }
    }
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
                    Rvalue::WrapSome(o, t)
                    | Rvalue::Cast(o, t)
                    | Rvalue::BitCast(o, t)
                    | Rvalue::WorkerTake(t, o) => {
                        *t = t.erase();
                        erase_op(o);
                    }
                    Rvalue::Use(o) => erase_op(o),
                    Rvalue::Variant(_, _, ops) => ops.iter_mut().for_each(erase_op),
                    Rvalue::Aggregate(t, ops) => {
                        *t = t.erase();
                        ops.iter_mut().for_each(erase_op)
                    }
                    Rvalue::RawArenaAlloc(t, a, b) => {
                        *t = t.erase();
                        erase_op(a);
                        erase_op(b);
                    }
                    Rvalue::New(t) | Rvalue::TakeException(t) | Rvalue::SizeOf(t, _) => {
                        *t = t.erase()
                    }
                    Rvalue::SetException(o, _) => erase_op(o),
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

/// Whether the debug format can show values of type `t` (`seen`: the structs and enums being
/// checked further up, which recursive types reach again).
fn debuggable(p: &hir::Program, t: &Ty, seen: &mut Vec<Ty>) -> bool {
    match t {
        Ty::Fn(..) | Ty::Ref(_) | Ty::Iface(..) => false,
        Ty::Nullable(e)
        | Ty::Array(e)
        | Ty::Fixed(e, _)
        | Ty::FixedN(e, _)
        | Ty::Lits(e, _)
        | Ty::Distinct(_, _, e)
        | Ty::Immutable(e) => debuggable(p, e, seen),
        Ty::Tuple(ts) => ts.iter().all(|t| debuggable(p, t, seen)),
        Ty::Union(_, ms) => ms.iter().all(|t| debuggable(p, t, seen)),
        Ty::Struct(..) | Ty::Enum(..) if seen.contains(t) => true,
        Ty::Struct(sid, _, args) => {
            seen.push(t.clone());
            let owner = Ty::type_owner(*sid);
            p.struct_def(*sid)
                .fields
                .iter()
                .all(|f| debuggable(p, &f.ty.subst(owner, args), seen))
        }
        Ty::Enum(eid, _) => {
            seen.push(t.clone());
            p.enums[eid.0 as usize]
                .variants
                .iter()
                .all(|v| v.fields.iter().all(|f| debuggable(p, &f.ty, seen)))
        }
        _ => true,
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

/// The type tables lowering reads: the program's, or a generic instance's substituted copy
/// (shared by the instances with the same substitution).
enum TypesRef<'a> {
    Borrowed(&'a Types),
    Shared(std::rc::Rc<Types>),
}

impl std::ops::Deref for TypesRef<'_> {
    type Target = Types;

    fn deref(&self) -> &Types {
        match self {
            TypesRef::Borrowed(t) => t,
            TypesRef::Shared(t) => t,
        }
    }
}

struct Cx<'a> {
    program: &'a hir::Program,
    /// The type checker's results; while lowering a generic instance, a copy with its type
    /// arguments substituted.
    types: TypesRef<'a>,
    /// Generic instances: a function and the substitutions it's seen through (its own type
    /// parameters, and its type's for methods of generic types); instance `i` is MIR
    /// function `program.fns.len() + i`.
    instances: Vec<(FnId, Subst)>,
    /// The substitutions of the instance being lowered (closures inside it share them).
    cur_subst: Subst,
    /// Named functions used as values: (function, the adapter that takes an environment).
    fn_values: Vec<(FnId, FnId)>,
    file: &'a SourceFile,
    /// The program's file; spans elsewhere (the prelude) get no location.
    file_id: ppl_diag::FileId,
    diags: Vec<Diagnostic>,
    /// Spans already reported as not implemented (lowering has no such gaps right now).
    #[allow(dead_code)]
    reported: HashSet<(u32, u32)>,
    /// The (implementing type, interface) pairs that need an itable, by itable index.
    itables: Vec<(StructId, StructId)>,
    /// The MIR id of the function being lowered.
    cur_mir: FnId,
    /// Generator step functions (by MIR id): the frame slots after the captures in their
    /// environment (the state, then every saved local).
    gen_frames: HashMap<FnId, Vec<Ty>>,
    /// While lowering a thread's entry: the files whose top-level code it leaves out (the
    /// main entry leaves out the worker modules; a worker the entry module and the others).
    skip_files: HashSet<ppl_diag::FileId>,
    /// Lowering the inline workers' entry: `__inlineWorkerMain()` runs before the loop.
    inline_entry: bool,
    /// The requested synthesized functions (see `Synth`) and their placeholder ids
    /// (`SYNTH_BASE + i`), fixed up once every function exists.
    synths: Vec<(Synth, FnId)>,
}

/// The core library's reflection functions, which calls replace with synthesized ones.
const REFLECT_FNS: &[&str] = &[
    "__jsonRead",
    "__jsonWrite",
    "__reflectType",
    "__reflectTypeOf",
    "__reflectGet",
    "__reflectSet",
    "__reflectShow",
];

/// The first placeholder id of a synthesized function.
const SYNTH_BASE: u32 = 0x4000_0000;

/// A function the compiler writes itself (no HIR), made on demand.
#[derive(Clone, PartialEq, Debug)]
enum Synth {
    /// A class instance's string form (`show_body`): `(this) -> string`.
    Show(Ty),
    /// Reflection metadata (spec §11.3): a `__TypeInfo<seen>` describing type `of`. `file` is
    /// the module asking (it sees its own private fields). `dynamic`: takes the `seen` value
    /// and describes its runtime class; otherwise takes nothing.
    Info {
        seen: Ty,
        of: Ty,
        file: ppl_diag::FileId,
        dynamic: bool,
    },
    /// `(obj: T, id: int) -> V`: reads reflected field `id` (see `field_id`).
    Get(Ty, Ty),
    /// `(obj: T, id: int, value: V)`: writes reflected field `id`.
    Set(Ty, Ty),
    /// `(obj: T, id: int) -> string`: a reflected field's string form.
    FieldShow(Ty),
    /// `(r: __JsonReader) -> T`: reads a `T` from JSON (spec §11.4).
    JsonRead(Ty),
    /// `(w: __JsonWriter, v: T)`: writes `v` as JSON.
    JsonWrite(Ty),
}

/// `s` as a JSON string literal (as `__jsonQuote` writes it).
fn json_quote(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 32 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A struct or class field's JSON key.
fn json_key(f: &hir::FieldDef) -> String {
    f.json_name.clone().unwrap_or_else(|| f.name.clone())
}

/// What a field accessor does.
enum Access {
    Get(Ty),
    Set(Ty),
    Show,
}

impl Access {
    fn new(set: bool, v: &Ty) -> Access {
        if set {
            Access::Set(v.clone())
        } else {
            Access::Get(v.clone())
        }
    }
}

/// The id reflection gives field `i` of struct or class `sid`: the class declaring it (for a
/// non-generic class, a base class for inherited fields) and the index, as `(class << 20) | i`.
fn field_id(p: &hir::Program, mut sid: StructId, i: usize) -> i64 {
    while p.struct_def(sid).generics.is_empty()
        && let Some(b) = p.struct_def(sid).base
        && p.struct_def(b).generics.is_empty()
        && p.struct_def(b).fields.len() > i
    {
        sid = b;
    }
    ((sid.0 as i64) << 20) | i as i64
}

/// The non-generic classes deriving from `sid`, directly or not.
fn subclasses(p: &hir::Program, sid: StructId) -> Vec<StructId> {
    (0..p.structs.len())
        .map(|i| StructId(i as u32))
        .filter(|s| {
            let d = p.struct_def(*s);
            *s != sid && d.is_class && d.generics.is_empty() && p.is_subclass(*s, sid)
        })
        .collect()
}

/// `T.kind` (and `TypeInfo.kind`) for type `t`.
fn kind_name(t: &Ty) -> &'static str {
    match t {
        Ty::Struct(..) => "struct",
        Ty::Class(..) => "class",
        Ty::Iface(..) => "interface",
        Ty::Enum(..) | Ty::Union(..) => "enum",
        Ty::Int(_) | Ty::Float(_) | Ty::Bool | Ty::Char | Ty::Str | Ty::Void => "primitive",
        Ty::Array(_) => "array",
        Ty::Fixed(..) => "fixed array",
        Ty::Tuple(_) => "tuple",
        Ty::Nullable(_) => "nullable",
        Ty::Fn(..) => "function",
        _ => "other",
    }
}

impl Cx<'_> {
    /// The MIR function for generic `id` seen through `subst`.
    /// Whether MIR function `id` (a HIR function or a generic instance) declares `throws`.
    fn throws(&self, id: FnId) -> bool {
        let base = self.program.fns.len();
        let i = id.0 as usize;
        let h = if i < base {
            id
        } else if let Some((f, _)) = self.instances.get(i - base) {
            *f
        } else {
            return false;
        };
        !self.types.fn_throws[h.0 as usize].is_empty()
    }

    /// The function giving the string form of a class instance of type `t` (spec §6.1):
    /// its runtime class's `toString(): string`, or the default `Name (field = value, …)`.
    fn show_fn(&mut self, t: &Ty) -> FnId {
        self.synth(Synth::Show(t.clone()))
    }

    /// The id of synthesized function `what`, requesting it the first time.
    fn synth(&mut self, what: Synth) -> FnId {
        if let Some((_, f)) = self.synths.iter().find(|(x, _)| *x == what) {
            return *f;
        }
        let f = FnId(SYNTH_BASE + self.synths.len() as u32);
        self.synths.push((what, f));
        f
    }

    /// A core-library class (by its name there, like `__TypeInfo`).
    fn core_struct(&self, name: &str) -> StructId {
        let i = self
            .program
            .structs
            .iter()
            .position(|d| d.name == name)
            .unwrap_or_else(|| panic!("core class `{name}`"));
        StructId(i as u32)
    }

    /// A class's own or inherited `toString(): string` (no parameters, doesn't throw).
    fn to_string_method(&self, sid: StructId) -> Option<FnId> {
        let m = self.program.struct_def(sid).method("toString")?;
        let sig = &self.types.sigs[m.0 as usize];
        let f = self.program.func(m);
        (sig.params.is_empty()
            && sig.ret.erase() == Ty::Str
            && f.generics.is_empty()
            && f.this.is_some()
            && self.types.fn_throws[m.0 as usize].is_empty())
        .then_some(m)
    }

    /// Builds synthesized function `id`.
    fn synth_function(&mut self, what: &Synth, id: FnId) -> mir::Function {
        let entry = self.program.entry;
        let hir_fn = self.program.func(entry);
        // Messages point at the type's declaration.
        let about = match what {
            Synth::Show(t) | Synth::Get(t, _) | Synth::Set(t, _) | Synth::FieldShow(t) => t,
            Synth::Info { of, .. } => of,
            Synth::JsonRead(t) | Synth::JsonWrite(t) => t,
        };
        let span = match about {
            Ty::Struct(sid, _, _) | Ty::Class(sid, _, _) => self.program.struct_def(*sid).name_span,
            _ => hir_fn.span,
        };
        self.cur_mir = id;
        let mut b = FnBuilder {
            cx: self,
            hir_fn,
            fn_id: entry,
            locals: Vec::new(),
            blocks: vec![(Vec::new(), None)],
            cur: BlockId(0),
            loops: Vec::new(),
            handlers: Vec::new(),
            finallies: Vec::new(),
            guards: Vec::new(),
            propagate: None,
            generator: None,
        };
        let (name, params, ret, value) = match what {
            Synth::Show(t) => {
                let this = b.temp(t.clone());
                let v = b.show_body(this, t, span);
                (format!("{t}.<show>"), vec![this], Ty::Str, Some(v))
            }
            Synth::Info {
                seen,
                of,
                file,
                dynamic,
            } => {
                let v = dynamic.then(|| b.temp(seen.clone()));
                let r = b.info_body(seen, of, *file, v, span);
                let ret = b.cx.type_info_ty(seen);
                (
                    format!("{of}.<typeinfo>"),
                    v.into_iter().collect(),
                    ret,
                    Some(r),
                )
            }
            Synth::Get(t, v) | Synth::Set(t, v) => {
                let obj = b.temp(t.clone());
                let fid = b.temp(Ty::INT);
                let set = matches!(what, Synth::Set(..));
                let value = set.then(|| b.temp(v.clone()));
                b.access_body(t, Access::new(set, v), obj, fid, value, span);
                let mut params = vec![obj, fid];
                params.extend(value);
                let (n, ret) = if set {
                    ("set", Ty::Void)
                } else {
                    ("get", v.clone())
                };
                (format!("{t}.<{n} {v}>"), params, ret, None)
            }
            Synth::JsonRead(t) => {
                let r = b.temp(b.cx.program.struct_ty(b.cx.core_struct("__JsonReader")));
                let v = b.json_read(r, t, span);
                (format!("{t}.<from json>"), vec![r], t.clone(), Some(v))
            }
            Synth::JsonWrite(t) => {
                let w = b.temp(b.cx.program.struct_ty(b.cx.core_struct("__JsonWriter")));
                let v = b.temp(t.clone());
                b.json_write(w, v, t, span);
                b.terminate(Terminator::Return(None));
                (format!("{t}.<to json>"), vec![w, v], Ty::Void, None)
            }
            Synth::FieldShow(t) => {
                let obj = b.temp(t.clone());
                let fid = b.temp(Ty::INT);
                b.access_body(t, Access::Show, obj, fid, None, span);
                (format!("{t}.<show field>"), vec![obj, fid], Ty::Str, None)
            }
        };
        if let Some(v) = value {
            b.terminate(Terminator::Return(Some(v)));
        }
        let blocks = b
            .blocks
            .into_iter()
            .map(|(stmts, term)| mir::BasicBlock {
                stmts,
                term: term.unwrap_or(Terminator::Unreachable),
            })
            .collect();
        mir::Function {
            name,
            library: true,
            params,
            ret,
            locals: b.locals,
            blocks,
        }
    }

    /// `__TypeInfo<seen>`.
    fn type_info_ty(&self, seen: &Ty) -> Ty {
        let sid = self.core_struct("__TypeInfo");
        self.program
            .struct_ty(sid)
            .subst(Ty::type_owner(sid), std::slice::from_ref(seen))
    }

    /// The type of field `field` of core class `class` seen through type arguments `args`.
    fn core_field_ty(&self, class: &str, field: &str, args: &[Ty]) -> (u32, Ty) {
        let sid = self.core_struct(class);
        let (i, f) = self
            .program
            .struct_def(sid)
            .field(field)
            .expect("core field");
        (i as u32, f.ty.subst(Ty::type_owner(sid), args))
    }

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
                library: true,
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
            library: true,
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
            handlers: Vec::new(),
            finallies: Vec::new(),
            guards: Vec::new(),
            propagate: None,
            generator: None,
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
        // A generator: dispatch on the state first (filled in once the body is lowered).
        let dispatch = match (f.generator, env, &f.closure) {
            (true, Some(e), Some(info)) => {
                b.generator = Some(GenState {
                    env: e,
                    captures: info.captures.len() as u32,
                    yields: Vec::new(),
                });
                let d = b.new_block();
                let start = b.new_block();
                b.terminate(Terminator::Goto(d));
                b.switch_to(start);
                Some((d, start))
            }
            _ => None,
        };
        let value = b.block(&f.body);
        if let Some((d, start)) = dispatch {
            b.finish_generator(d, start, &sig.ret, f.span);
        }
        let ret = if f.is_entry {
            Ty::Void
        } else {
            sig.ret.clone()
        };
        if f.is_entry {
            b.exit_hooks(f.span);
        }
        let term = match ret {
            Ty::Void | Ty::Never | Ty::Error => Terminator::Return(None),
            // No value where one is needed: the body diverged, so the end can't be reached.
            _ if value == Operand::Const(Const::Void) => Terminator::Unreachable,
            _ => Terminator::Return(Some(value)),
        };
        b.terminate(term);
        if let Some(pb) = b.propagate {
            b.switch_to(pb);
            b.propagate_exception(&ret, f.span);
        }
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
            library: f.span.file != b.cx.file_id,
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
    /// Where a raised exception goes: the enclosing `try`s' dispatch blocks, innermost last.
    handlers: Vec<BlockId>,
    /// The enclosing `finally` blocks, innermost last, with the loop and handler depths
    /// outside their `try` (`return`, `break` and `continue` run them on the way out).
    /// The expressions live in the HIR, which outlives the builder.
    finallies: Vec<(Fin, usize, usize)>,
    /// The handlers of the panic guards around the code being lowered, outermost first: a
    /// generator's `yield` leaves them (the step returns) and re-arms them on resuming.
    guards: Vec<BlockId>,
    /// The block that passes an exception on to the caller (made when first needed).
    propagate: Option<BlockId>,
    /// A generator step function's state machine.
    generator: Option<GenState>,
}

/// Code that runs whenever control leaves a region (`return`, `break`, `continue`).
#[derive(Clone, Copy)]
enum Fin {
    /// A `finally` block (its `try` is also a panic guard) and the number of guards outside
    /// it.
    Finally(*const Expr, usize),
    /// An `arena` block: its named locals (`a..b`) and the temporaries from `c` on are
    /// cleared, then it's closed.
    Arena(usize, usize, usize, Span),
}

/// A generator step function: its environment holds the captures, then the state (0: not
/// started, `k`: after the `k`th yield, -1: done), then a slot per local.
struct GenState {
    env: Local,
    captures: u32,
    /// Per `yield`: the block that saves the frame and returns the value (filled in at the
    /// end, when every local is known), the value, and where the next call resumes.
    yields: Vec<(BlockId, Local, BlockId)>,
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
        if span.file != self.cx.file_id {
            return Loc {
                span: Some(span),
                ..Loc::default()
            };
        }
        let (line, col) = self.cx.file.line_col(span.start);
        Loc {
            line: line as u32,
            col: col as u32,
            span: Some(span),
        }
    }

    fn ty(&self, e: &Expr) -> Ty {
        self.cx.types.expr(e.id).clone()
    }

    /// Whether a top-level statement belongs to a module this thread's entry leaves out.
    /// (Static initializers run in every thread.)
    fn skipped(&self, s: &Stmt) -> bool {
        let span = match s {
            Stmt::Let { span, .. } => *span,
            Stmt::Expr(e) => {
                if let ExprKind::Call { callee, .. } = &e.kind
                    && let ExprKind::Fn(id) = callee.kind
                    && self.cx.program.static_inits.contains(&id)
                {
                    return false;
                }
                e.span
            }
        };
        self.cx.skip_files.contains(&span.file)
    }

    /// `T.name`, `T.kind`, `T.fields`, `T.methods` of a concrete type: constants (an array
    /// of `FieldInfo`/`MethodInfo` structs for the lists).
    fn type_info(&mut self, t: &Ty, member: hir::TypeMember, elem: &Ty, span: Span) -> Operand {
        use hir::TypeMember as M;
        let s = |v: String| Operand::Const(Const::Str(v));
        let def = match t {
            Ty::Struct(sid, _, args) | Ty::Class(sid, _, args) | Ty::Iface(sid, _, args) => {
                Some((self.cx.program.struct_def(*sid), *sid, args.clone()))
            }
            _ => None,
        };
        // Private members only in the declaring module (spec §11.3).
        let visible = |n: &str, d: &hir::StructDef| {
            !n.starts_with("__") && (!d.private.contains(n) || d.name_span.file == span.file)
        };
        match member {
            M::Name => s(t.to_string()),
            M::Kind => s(kind_name(t).to_string()),
            M::Fields => {
                let fields: Vec<(String, String)> = match &def {
                    Some((d, sid, args)) => d
                        .fields
                        .iter()
                        .filter(|f| visible(&f.name, d))
                        .map(|f| {
                            (
                                f.name.clone(),
                                f.ty.subst(Ty::type_owner(*sid), args).to_string(),
                            )
                        })
                        .collect(),
                    None => Vec::new(),
                };
                let vals = fields
                    .into_iter()
                    .map(|(n, ty)| {
                        self.rvalue_to_temp(
                            Rvalue::Aggregate(elem.clone(), vec![s(n), s(ty)]),
                            elem.clone(),
                            span,
                        )
                    })
                    .collect();
                self.rvalue_to_temp(
                    Rvalue::ArrayNew(elem.clone(), vals),
                    Ty::Array(Box::new(elem.clone())),
                    span,
                )
            }
            M::Methods => {
                let names: Vec<String> = match &def {
                    Some((d, _, _)) => d
                        .methods
                        .iter()
                        .filter(|(n, _)| visible(n, d))
                        .map(|(n, _)| n.clone())
                        .collect(),
                    None => Vec::new(),
                };
                let vals = names
                    .into_iter()
                    .map(|n| {
                        self.rvalue_to_temp(
                            Rvalue::Aggregate(elem.clone(), vec![s(n)]),
                            elem.clone(),
                            span,
                        )
                    })
                    .collect();
                self.rvalue_to_temp(
                    Rvalue::ArrayNew(elem.clone(), vals),
                    Ty::Array(Box::new(elem.clone())),
                    span,
                )
            }
        }
    }

    /// A type written in the code, seen through the current instance's type arguments.
    fn mono(&self, t: &Ty) -> Ty {
        let mut t = t.clone();
        for (owner, args) in &self.cx.cur_subst {
            t = t.subst(*owner, args);
        }
        t
    }

    /// `arena { body }`: opens an arena, and closes it on every way out (the end, `return`,
    /// `break`/`continue`, exceptions); panics close it in the runtime.
    fn arena_block(
        &mut self,
        body: &hir::Block,
        named: (usize, usize),
        ty: Ty,
        span: Span,
    ) -> Operand {
        let result = has_value(&ty).then(|| Place::local(self.temp(ty.clone())));
        self.builtin_call(Builtin::ArenaEnter, span);
        let first = (named.0, named.1, self.locals.len());
        let outer_handlers = self.handlers.len();
        self.finallies.push((
            Fin::Arena(first.0, first.1, first.2, span),
            self.loops.len(),
            outer_handlers,
        ));
        let on_exception = self.new_block();
        self.handlers.push(on_exception);
        let v = self.block(body);
        if let Some(r) = &result {
            self.assign(r.clone(), Rvalue::Use(v), span);
        }
        self.handlers.pop();
        self.finallies.pop();
        self.arena_exit(first, span);
        let done = self.new_block();
        self.terminate(Terminator::Goto(done));
        // An exception leaves the block: close the arena (moving the exception out of it) and
        // pass it on.
        self.switch_to(on_exception);
        self.clear_arena_locals(first, span);
        self.builtin_call(Builtin::ArenaExitRescue, span);
        let target = self.raise_target();
        self.terminate(Terminator::Goto(target));
        self.switch_to(done);
        match result {
            Some(r) => Operand::Copy(r),
            None => Operand::Const(Const::Void),
        }
    }

    /// Clears the locals an `arena` block declared (its named locals and its temporaries;
    /// they'd point into freed memory, and the runtime checks no root still does) and closes
    /// the arena.
    fn arena_exit(&mut self, locals: (usize, usize, usize), span: Span) {
        self.clear_arena_locals(locals, span);
        self.builtin_call(Builtin::ArenaExit, span);
    }

    fn clear_arena_locals(&mut self, (a, b, temps): (usize, usize, usize), span: Span) {
        for i in (a..b).chain(temps..self.locals.len()) {
            let t = self.locals[i].ty.clone();
            if self.cx.program.holds_refs(&t) {
                self.assign(
                    Place::local(Local(i as u32)),
                    Rvalue::Use(Operand::Const(Const::Undef(t))),
                    span,
                );
            }
        }
    }

    fn builtin_call(&mut self, b: Builtin, span: Span) {
        let loc = self.loc(span);
        let next = self.new_block();
        self.terminate(Terminator::Call {
            callee: Callee::Builtin(b),
            args: Vec::new(),
            dest: None,
            next,
            loc,
        });
        self.switch_to(next);
    }

    /// `object` (a `ptr<T>`) in a local, to read or write through.
    fn ptr_local(&mut self, object: &Expr) -> Local {
        let t = self.ty_of(object).clone();
        let v = self.expr(object);
        let l = self.temp(t);
        self.assign(Place::local(l), Rvalue::Use(v), object.span);
        l
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
        if self.blocks[self.cur.0 as usize].1.is_some() {
            return;
        }
        let throws = match &term {
            Terminator::Call {
                callee:
                    Callee::Fn(id) | Callee::Virtual { decl: id, .. } | Callee::Iface { decl: id, .. },
                ..
            } => self.cx.throws(*id),
            // A function value may throw whatever its type says, or (generic) anything.
            Terminator::Call {
                callee: Callee::Indirect,
                ..
            } => true,
            _ => false,
        };
        let Terminator::Call {
            callee,
            args,
            dest,
            next,
            loc,
        } = term
        else {
            self.blocks[self.cur.0 as usize].1 = Some(term);
            return;
        };
        if !throws {
            self.blocks[self.cur.0 as usize].1 = Some(Terminator::Call {
                callee,
                args,
                dest,
                next,
                loc,
            });
            return;
        }
        // A throwing call: afterwards, a pending exception goes to the handler.
        let check = self.new_block();
        self.blocks[self.cur.0 as usize].1 = Some(Terminator::Call {
            callee,
            args,
            dest,
            next: check,
            loc,
        });
        let pending = self.temp(Ty::Bool);
        let target = self.raise_target();
        let blk = &mut self.blocks[check.0 as usize];
        blk.0.push(mir::Statement {
            place: Place::local(pending),
            rvalue: Rvalue::ExceptionPending,
            loc,
        });
        blk.1 = Some(Terminator::Branch {
            cond: Operand::Copy(Place::local(pending)),
            then: target,
            else_: next,
        });
    }

    /// Where an exception raised here goes.
    fn raise_target(&mut self) -> BlockId {
        if let Some(h) = self.handlers.last() {
            return *h;
        }
        match self.propagate {
            Some(b) => b,
            None => {
                let b = self.new_block();
                self.propagate = Some(b);
                b
            }
        }
    }

    /// The propagate block: return to the caller with the exception pending, or at the top
    /// level, report it and exit.
    fn propagate_exception(&mut self, ret: &Ty, span: Span) {
        let top = self.hir_fn.is_entry || self.cx.program.static_inits.contains(&self.fn_id);
        if !top {
            let term = match ret {
                Ty::Void | Ty::Never | Ty::Error => Terminator::Return(None),
                t => Terminator::Return(Some(Operand::Const(Const::Undef(t.clone())))),
            };
            self.terminate(term);
            return;
        }
        let Some(err) = self.cx.program.error_class else {
            self.terminate(Terminator::Unreachable);
            return;
        };
        let _ = err;
        let (raw, tag) = self.take_exception(span);
        let (class, msg) = self.exception_text(&raw, &tag, span);
        self.terminate(Terminator::Uncaught { class, msg });
    }

    /// Takes the pending exception: the `Error` object or the boxed throwable, and its tag.
    fn take_exception(&mut self, span: Span) -> (Place, Place) {
        let tag = Place::local(self.temp(Ty::Int(IntTy::U32)));
        self.assign(tag.clone(), Rvalue::ExceptionTag, span);
        let err = self.cx.program.error_class.expect("prelude");
        let err_ty = self.cx.program.struct_def(err).self_ty(err);
        let raw = Place::local(self.temp(err_ty.clone()));
        self.assign(raw.clone(), Rvalue::TakeException(err_ty), span);
        (raw, tag)
    }

    fn set_exception(&mut self, v: Operand, tag: Operand, span: Span) {
        let t = self.temp(Ty::Void);
        self.assign(Place::local(t), Rvalue::SetException(v, tag), span);
    }

    fn tag_is(&mut self, tag: &Place, k: u32, span: Span) -> Operand {
        self.rvalue_to_temp(
            Rvalue::Binary(
                mir::BinOp::Eq,
                Operand::Copy(tag.clone()),
                Operand::Const(Const::Int(k as i128, IntTy::U32)),
            ),
            Ty::Bool,
            span,
        )
    }

    /// A taken exception's type name and message.
    fn exception_text(&mut self, raw: &Place, tag: &Place, span: Span) -> (Operand, Operand) {
        let p = self.cx.program;
        let err = p.error_class.expect("prelude");
        let name = Place::local(self.temp(Ty::Str));
        let msg = Place::local(self.temp(Ty::Str));
        let done = self.new_block();
        // Lightweight throwables, by tag.
        for (i, t) in p.throwables().into_iter().enumerate() {
            let is = self.tag_is(tag, i as u32 + 1, span);
            let yes = self.new_block();
            let no = self.new_block();
            self.terminate(Terminator::Branch {
                cond: is,
                then: yes,
                else_: no,
            });
            self.switch_to(yes);
            let boxed = self.temp(Ty::Ref(Box::new(t.clone())));
            self.assign(
                Place::local(boxed),
                Rvalue::Use(Operand::Copy(raw.clone())),
                span,
            );
            let (tname, text) = match &t {
                Ty::Struct(sid, n, _) => {
                    let m = p
                        .struct_def(*sid)
                        .method("message")
                        .expect("checked by conformance");
                    let v = self.call_fn_value(
                        m,
                        vec![Operand::Copy(Place::local(boxed))],
                        Ty::Str,
                        span,
                    );
                    (n.to_string(), v)
                }
                Ty::Enum(_, n) => {
                    let spec = ppl_sema::format::parse("?").expect("valid spec");
                    let v = self.rvalue_to_temp(
                        Rvalue::Format(Operand::Copy(Place::deref(boxed)), spec),
                        Ty::Str,
                        span,
                    );
                    (n.to_string(), v)
                }
                _ => (String::new(), Operand::Const(Const::Str(String::new()))),
            };
            self.assign(
                name.clone(),
                Rvalue::Use(Operand::Const(Const::Str(tname))),
                span,
            );
            self.assign(msg.clone(), Rvalue::Use(text), span);
            self.terminate(Terminator::Goto(done));
            self.switch_to(no);
        }
        // An `Error`: its class (the subclasses of `Error`, deepest first) and `message`.
        let mut classes: Vec<(usize, StructId)> = (0..p.structs.len())
            .map(|i| StructId(i as u32))
            .filter(|s| {
                let d = p.struct_def(*s);
                d.is_class && d.generics.is_empty() && p.is_subclass(*s, err)
            })
            .map(|s| {
                let mut depth = 0;
                let mut c = s;
                while let Some(b) = p.struct_def(c).base {
                    depth += 1;
                    c = b;
                }
                (depth, s)
            })
            .collect();
        classes.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.0.cmp(&b.1.0)));
        let text = Operand::Copy(raw.field(0));
        self.assign(msg.clone(), Rvalue::Use(text), span);
        for (_, s) in classes {
            let is = self.rvalue_to_temp(
                Rvalue::InstanceOf(Operand::Copy(raw.clone()), s),
                Ty::Bool,
                span,
            );
            let yes = self.new_block();
            let no = self.new_block();
            self.terminate(Terminator::Branch {
                cond: is,
                then: yes,
                else_: no,
            });
            self.switch_to(yes);
            let n = p.struct_def(s).name.clone();
            self.assign(
                name.clone(),
                Rvalue::Use(Operand::Const(Const::Str(n.as_str().into()))),
                span,
            );
            self.terminate(Terminator::Goto(done));
            self.switch_to(no);
        }
        self.terminate(Terminator::Unreachable);
        self.switch_to(done);
        (Operand::Copy(name), Operand::Copy(msg))
    }

    /// Runs the enclosing `finally` blocks (innermost first) whose `try` is inside the
    /// innermost `min_loops` loops… i.e. those entered at a loop depth of at least
    /// `min_loops`, on the way out of them.
    fn run_finallies(&mut self, min_loops: usize) {
        let saved_f = self.finallies.clone();
        let saved_h = self.handlers.clone();
        let saved_g = self.guards.clone();
        for i in (0..saved_f.len()).rev() {
            let (fin, loops, handlers) = saved_f[i];
            if loops < min_loops {
                break;
            }
            self.finallies.truncate(i);
            self.handlers.truncate(handlers);
            match fin {
                Fin::Finally(fin, outer_guards) => {
                    self.guards.truncate(outer_guards);
                    self.unguard();
                    // SAFETY: the expression is part of the HIR, which outlives this builder.
                    let fin = unsafe { &*fin };
                    self.expr(fin);
                }
                Fin::Arena(a, b, c, span) => self.arena_exit((a, b, c), span),
            }
        }
        self.finallies = saved_f;
        self.handlers = saved_h;
        self.guards = saved_g;
    }

    /// Ends a generator's step: `null` from now on.
    fn generator_done(&mut self, span: Span) {
        let g = self.generator.as_ref().expect("a generator");
        let state = Place::deref(g.env).field(g.captures);
        self.assign(
            state,
            Rvalue::Use(Operand::Const(Const::Int(-1, IntTy::I64))),
            span,
        );
        let ret = self.cx.types.sigs[self.hir_fn_id().0 as usize].ret.clone();
        self.terminate(Terminator::Return(Some(Operand::Const(Const::Null(ret)))));
    }

    /// The HIR function being lowered.
    fn hir_fn_id(&self) -> FnId {
        self.fn_id
    }

    /// The frame slots of every local (except the environment) after the state.
    fn frame_locals(&self) -> Vec<(Local, u32)> {
        let g = self.generator.as_ref().expect("a generator");
        let mut out = Vec::new();
        let mut slot = g.captures + 1;
        for (i, l) in self.locals.iter().enumerate() {
            if i as u32 == g.env.0 || !has_value(&l.ty) {
                continue;
            }
            out.push((Local(i as u32), slot));
            slot += 1;
        }
        out
    }

    /// Fills in the dispatch, the saves at each `yield`, and the end of a generator.
    fn finish_generator(&mut self, dispatch: BlockId, start: BlockId, ret: &Ty, span: Span) {
        // Falling off the end finishes it.
        if self.blocks[self.cur.0 as usize].1.is_none() {
            self.generator_done(span);
        }
        let frame = self.frame_locals();
        let (env, captures, yields) = {
            let g = self.generator.as_ref().expect("a generator");
            (g.env, g.captures, g.yields.clone())
        };
        let state_place = Place::deref(env).field(captures);
        for (k, (save, value, _)) in yields.iter().enumerate() {
            self.switch_to(*save);
            for (l, slot) in &frame {
                self.assign(
                    Place::deref(env).field(*slot),
                    Rvalue::Use(Operand::Copy(Place::local(*l))),
                    span,
                );
            }
            self.assign(
                state_place.clone(),
                Rvalue::Use(Operand::Const(Const::Int(k as i128 + 1, IntTy::I64))),
                span,
            );
            let elem = self.locals[value.0 as usize].ty.clone();
            let v = self.convert(Operand::Copy(Place::local(*value)), &elem, ret, span);
            self.terminate(Terminator::Return(Some(v)));
        }
        // Dispatch: 0 starts, k resumes after yield k (restoring the frame), else done.
        self.switch_to(dispatch);
        let state = self.rvalue_to_temp(
            Rvalue::Use(Operand::Copy(state_place.clone())),
            Ty::Int(IntTy::I64),
            span,
        );
        let targets: Vec<(i128, Option<BlockId>)> = std::iter::once((0, None))
            .chain(
                yields
                    .iter()
                    .enumerate()
                    .map(|(k, (_, _, resume))| (k as i128 + 1, Some(*resume))),
            )
            .collect();
        for (k, resume) in targets {
            let is = self.rvalue_to_temp(
                Rvalue::Binary(
                    mir::BinOp::Eq,
                    state.clone(),
                    Operand::Const(Const::Int(k, IntTy::I64)),
                ),
                Ty::Bool,
                span,
            );
            let yes = self.new_block();
            let no = self.new_block();
            self.terminate(Terminator::Branch {
                cond: is,
                then: yes,
                else_: no,
            });
            self.switch_to(yes);
            match resume {
                None => self.terminate(Terminator::Goto(start)),
                Some(r) => {
                    for (l, slot) in &frame {
                        self.assign(
                            Place::local(*l),
                            Rvalue::Use(Operand::Copy(Place::deref(env).field(*slot))),
                            span,
                        );
                    }
                    self.terminate(Terminator::Goto(r));
                }
            }
            self.switch_to(no);
        }
        self.terminate(Terminator::Return(Some(Operand::Const(Const::Null(
            ret.clone(),
        )))));
        // The environment's full type, for the closure that creates it.
        let mut slots = vec![Ty::Int(IntTy::I64)];
        slots.extend(
            frame
                .iter()
                .map(|(l, _)| self.locals[l.0 as usize].ty.clone()),
        );
        if let Ty::Ref(t) = &self.locals[env.0 as usize].ty
            && let Ty::Tuple(caps) = &**t
        {
            let mut all: Vec<Ty> = caps.to_vec();
            all.extend(slots.iter().cloned());
            self.locals[env.0 as usize].ty = Ty::Ref(Box::new(Ty::Tuple(all)));
        }
        let me = self.cx.cur_mir;
        self.cx.gen_frames.insert(me, slots);
    }

    /// Leaves the innermost panic-guarded region.
    fn unguard(&mut self) {
        let next = self.new_block();
        self.terminate(Terminator::Unguard(next));
        self.switch_to(next);
    }

    /// `try body catch (…) … finally { … }`
    fn try_expr(
        &mut self,
        body: &Expr,
        catches: &[hir::CatchClause],
        finally: Option<&Expr>,
        ty: Ty,
        span: Span,
    ) -> Operand {
        let result = has_value(&ty).then(|| Place::local(self.temp(ty.clone())));
        let outer_handlers = self.handlers.len();
        let dispatch = self.new_block();
        let done = self.new_block();
        if let Some(f) = finally {
            self.finallies.push((
                Fin::Finally(f as *const Expr, self.guards.len()),
                self.loops.len(),
                outer_handlers,
            ));
            // A panic in the body or a catch clause runs `finally` on its way out.
            let body = self.new_block();
            let handler = self.new_block();
            self.terminate(Terminator::Guard { body, handler });
            let saved_h = std::mem::take(&mut self.handlers);
            let saved_f = std::mem::take(&mut self.finallies);
            let saved_l = std::mem::take(&mut self.loops);
            let saved_g = std::mem::take(&mut self.guards);
            self.switch_to(handler);
            self.expr(f);
            self.terminate(Terminator::Resume);
            self.handlers = saved_h;
            self.finallies = saved_f;
            self.loops = saved_l;
            self.guards = saved_g;
            self.guards.push(handler);
            self.switch_to(body);
        }
        self.handlers.push(dispatch);
        let v = self.expr(body);
        if let Some(r) = &result
            && self.ty(body) != Ty::Never
        {
            self.assign(r.clone(), Rvalue::Use(v), span);
        }
        self.handlers.pop();
        self.terminate(Terminator::Goto(done));
        // An exception in the body: try the catch clauses in order.
        self.switch_to(dispatch);
        let (exc, tag) = self.take_exception(span);
        // Exceptions in the catch clauses (and the rethrow) still run `finally`.
        let fin_exc = finally.map(|_| self.new_block());
        if let Some(b) = fin_exc {
            self.handlers.push(b);
        }
        for c in catches {
            let next = self.new_block();
            self.catch_pattern(&c.pat, &exc, &tag, next);
            let v = self.expr(&c.body);
            if let Some(r) = &result
                && self.ty(&c.body) != Ty::Never
            {
                self.assign(r.clone(), Rvalue::Use(v), c.span);
            }
            self.terminate(Terminator::Goto(done));
            self.switch_to(next);
        }
        // No clause matched: rethrow.
        self.set_exception(Operand::Copy(exc), Operand::Copy(tag), span);
        let target = self.raise_target();
        self.terminate(Terminator::Goto(target));
        if fin_exc.is_some() {
            self.handlers.pop();
            self.finallies.pop();
        }
        if finally.is_some() {
            self.guards.pop();
        }
        self.switch_to(done);
        if let Some(f) = finally {
            self.unguard();
            self.expr(f);
        }
        if let (Some(b), Some(f)) = (fin_exc, finally) {
            let after = self.new_block();
            self.terminate(Terminator::Goto(after));
            self.switch_to(b);
            self.unguard();
            let (held, htag) = self.take_exception(span);
            self.expr(f);
            self.set_exception(Operand::Copy(held), Operand::Copy(htag), span);
            let target = self.raise_target();
            self.terminate(Terminator::Goto(target));
            self.switch_to(after);
        }
        match result {
            Some(p) => Operand::Copy(p),
            None => Operand::Const(Const::Void),
        }
    }

    /// Matches a `catch` clause against the taken exception; continues at `fail` otherwise.
    fn catch_pattern(&mut self, pat: &hir::Pattern, exc: &Place, tag: &Place, fail: BlockId) {
        let span = pat.span;
        let throwable = match &pat.kind {
            hir::PatKind::Type { ty, bind, fields } => self
                .cx
                .program
                .throw_tag(ty)
                .map(|k| (k, ty.clone(), *bind, fields)),
            _ => None,
        };
        if let Some((k, ty, bind, fields)) = throwable {
            let is = self.tag_is(tag, k, span);
            let yes = self.new_block();
            self.terminate(Terminator::Branch {
                cond: is,
                then: yes,
                else_: fail,
            });
            self.switch_to(yes);
            let boxed = self.temp(Ty::Ref(Box::new(ty)));
            self.assign(
                Place::local(boxed),
                Rvalue::Use(Operand::Copy(exc.clone())),
                span,
            );
            let place = Place::deref(boxed);
            if let Some(l) = bind {
                self.bind(l, &place, span);
            }
            if let Some(fps) = fields {
                let idx = self.cx.types.pat_fields[&pat.id].clone();
                for (f, i) in fps.iter().zip(idx) {
                    self.match_pattern(&f.pat, &place.field(i), fail);
                }
            }
            return;
        }
        if !matches!(pat.kind, hir::PatKind::Wild) {
            // Everything else looks at an `Error` object.
            let is = self.tag_is(tag, 0, span);
            let yes = self.new_block();
            self.terminate(Terminator::Branch {
                cond: is,
                then: yes,
                else_: fail,
            });
            self.switch_to(yes);
        }
        self.match_pattern(pat, exc, fail);
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
        let top = self.hir_fn.is_entry && std::ptr::eq(b, &self.hir_fn.body);
        for s in &b.stmts {
            if top && self.skipped(s) {
                continue;
            }
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
            Some(t) if top && self.cx.skip_files.contains(&t.span.file) => {
                Operand::Const(Const::Void)
            }
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
        // A function returning a subclass reference is the same function value.
        if let (Ty::Fn(p1, r1, _), Ty::Fn(p2, r2, _)) = (from, to)
            && p1 == p2
            && class_ref(r1)
            && class_ref(r2)
        {
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
            ExprKind::Int(v, _) => int_const(*v as i128, &ty.erase()),
            ExprKind::Float(v, _) => match ty.erase() {
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
            ExprKind::Unsafe(b) => self.block(b),
            ExprKind::Arena { body, first, end } => {
                self.arena_block(body, (*first as usize, *end as usize), ty, e.span)
            }
            ExprKind::RawArenaAlloc {
                ty: t,
                arena,
                count,
            } => {
                let h = self.expr(arena);
                let n = self.expr(count);
                let t = self.mono(t);
                self.rvalue_to_temp(Rvalue::RawArenaAlloc(t, h, n), ty, e.span)
            }
            ExprKind::PtrOf(inner) => {
                let place = self.place_of(inner);
                self.rvalue_to_temp(Rvalue::Ref(place), ty, e.span)
            }
            ExprKind::SizeOf { ty: t, align } => {
                let t = self.mono(t);
                self.rvalue_to_temp(Rvalue::SizeOf(t, *align), ty, e.span)
            }
            ExprKind::BitCast(inner, to) => {
                let v = self.expr(inner);
                let to = self.mono(to);
                self.rvalue_to_temp(Rvalue::BitCast(v, to), ty, e.span)
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
            ExprKind::FnAddr(id) => self.rvalue_to_temp(Rvalue::FnAddr(*id), ty, e.span),
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
                        // A struct method's `this` is a reference: the closure keeps it.
                        let struct_this = self.hir_fn.this.is_some_and(|(this, sid)| {
                            let d = self.cx.program.struct_def(sid);
                            this == l && !d.is_class && !d.is_interface
                        });
                        if c.by_ref || struct_this {
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
            ExprKind::Comptime { f, limit } => {
                let fid = if in_generic_context(self.cx.program, *f) {
                    let subst = self.cx.cur_subst.clone();
                    self.cx.instance(*f, subst)
                } else {
                    *f
                };
                self.rvalue_to_temp(Rvalue::Comptime(fid, *limit), ty, e.span)
            }
            ExprKind::Fn(_) | ExprKind::Builtin(_) | ExprKind::Overloaded(_) | ExprKind::Error => {
                Operand::Const(Const::Void)
            }
            ExprKind::NamedArg { value, .. } | ExprKind::Spread(value) => self.expr(value),
            ExprKind::Unary(_, inner) if self.cx.types.op_calls.contains_key(&e.id) => {
                let m = self.cx.types.op_calls[&e.id];
                self.user_call(e, m, inner, &[], ty)
            }
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
                op: None,
                target: hir::Target::Index { object, index },
                value,
            } if self.cx.types.op_calls.contains_key(&e.id) => {
                let m = self.cx.types.op_calls[&e.id];
                self.user_call(e, m, object, &[index, value], Ty::Void);
                Operand::Const(Const::Void)
            }
            ExprKind::Assign {
                op: Some(_),
                target,
                value,
            } if self.cx.types.op_calls.contains_key(&e.id) => {
                // `v += w` with `operator +`: `v = v + w`.
                let m = self.cx.types.op_calls[&e.id];
                let mut checks = Vec::new();
                let place = self.target_place(target, &mut checks);
                self.emit_checks(&checks);
                let t = self.place_ty(&place);
                let recv = self.receiver(&place, &t, e.span);
                let mut ops = vec![recv];
                let rest = self.call_operands(e, m, std::slice::from_ref(&**value));
                ops.extend(rest);
                let v = self.call_fn_value(m, ops, t, e.span);
                self.assign(place, Rvalue::Use(v), e.span);
                Operand::Const(Const::Void)
            }
            ExprKind::Assign {
                op,
                target: target @ hir::Target::Field { id, object, .. },
                value,
            } if self.cx.types.setters.contains_key(id) => {
                // `obj.prop = v` / `obj.prop += v` through accessors.
                let setter = self.cx.types.setters[id];
                let rt = self.ty_of(object).clone();
                let setter = self.method_of(setter, &rt);
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
                        let getter = self.method_of(getter, &rt);
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
                        let mut v = self.expr(value);
                        if *op == ast::BinOp::Add && self.place_ty(&place).erase() == Ty::Str {
                            let vt = self.cx.types.final_ty(value.id).clone();
                            v = self.stringify(v, &vt, value.span);
                        }
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
                let rt = self.ty_of(object).clone();
                let g = self.method_of(g, &rt);
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
                let nfields = self.cx.program.struct_def(sid).fields.len();
                let mut ops: Vec<Option<Operand>> = vec![None; nfields.max(order.len())];
                // Evaluate in source order, store in declaration order.
                for (f, slot) in fields.iter().zip(&order) {
                    let v = self.expr(&f.value);
                    ops[*slot as usize] = Some(v);
                }
                // Optional fields left out are `null`.
                let owner = Ty::type_owner(sid);
                let targs = ty.type_args().to_vec();
                let ops = ops
                    .into_iter()
                    .enumerate()
                    .map(|(i, o)| {
                        o.unwrap_or_else(|| {
                            let ft = &self.cx.program.struct_def(sid).fields[i].ty;
                            Operand::Const(Const::Null(ft.subst(owner, &targs)))
                        })
                    })
                    .collect();
                self.rvalue_to_temp(Rvalue::Aggregate(ty.clone(), ops), ty, e.span)
            }
            ExprKind::Index { object, index } if *self.ty_of(object) == Ty::Str => {
                let s = self.expr(object);
                let i = self.expr(index);
                self.rvalue_to_temp(Rvalue::StrCharAt(s, i), ty, e.span)
            }
            ExprKind::Index { object, index } if self.cx.types.op_calls.contains_key(&e.id) => {
                let m = self.cx.types.op_calls[&e.id];
                self.user_call(e, m, object, &[index], ty)
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
            // `[v; N]` in an instance: its type has the length.
            ExprKind::RepeatN { value, .. } => {
                let v = self.expr(value);
                let Ty::Fixed(_, n) = ty else {
                    return Operand::Const(Const::Void);
                };
                self.rvalue_to_temp(Rvalue::Repeat(v, n), ty, e.span)
            }
            ExprKind::TypeInfo { ty, member, elem } => {
                let t = self.mono(ty);
                self.type_info(&t, *member, elem, e.span)
            }
            ExprKind::TypeValue(_) => Operand::Const(Const::Void),
            // A `comptime` parameter: this instance's constant.
            ExprKind::ConstParam(t) => match self.mono(t) {
                Ty::Lits(_, vs) => match vs.first() {
                    Some(LitVal::Int(v)) => int_const(*v, &ty.erase()),
                    Some(LitVal::Bool(b)) => Operand::Const(Const::Bool(*b)),
                    Some(LitVal::Str(s)) => Operand::Const(Const::Str(s.to_string())),
                    None => Operand::Const(Const::Void),
                },
                _ => Operand::Const(Const::Void),
            },
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
            ExprKind::TryOpt(inner) | ExprKind::TryBang(inner) => {
                let bang = matches!(e.kind, ExprKind::TryBang(_));
                let result = has_value(&ty).then(|| Place::local(self.temp(ty.clone())));
                let dispatch = self.new_block();
                let done = self.new_block();
                self.handlers.push(dispatch);
                let v = self.expr(inner);
                let it = self.ty(inner);
                self.handlers.pop();
                if let Some(r) = &result
                    && it != Ty::Never
                {
                    let v = self.convert(v, &it, &ty, e.span);
                    self.assign(r.clone(), Rvalue::Use(v), e.span);
                }
                self.terminate(Terminator::Goto(done));
                self.switch_to(dispatch);
                let (exc, tag) = self.take_exception(e.span);
                if bang {
                    // `try!`: the error becomes a panic with its message.
                    let (_, msg) = self.exception_text(&exc, &tag, e.span);
                    let loc = self.loc(e.span);
                    self.terminate(Terminator::Panic { msg, loc });
                } else {
                    if let Some(r) = &result {
                        self.assign(
                            r.clone(),
                            Rvalue::Use(Operand::Const(Const::Null(ty.clone()))),
                            e.span,
                        );
                    }
                    self.terminate(Terminator::Goto(done));
                }
                self.switch_to(done);
                match result {
                    Some(p) => Operand::Copy(p),
                    None => Operand::Const(Const::Void),
                }
            }
            ExprKind::Yield(inner) => {
                let v = self.expr(inner);
                let it = self.ty(inner);
                let value = self.temp(it);
                self.assign(Place::local(value), Rvalue::Use(v), e.span);
                // Leaving the step: out of the panic guards around it.
                for _ in 0..self.guards.len() {
                    self.unguard();
                }
                let save = self.new_block();
                let resume = self.new_block();
                self.terminate(Terminator::Goto(save));
                self.generator
                    .as_mut()
                    .expect("resolver allows `yield` only in generators")
                    .yields
                    .push((save, value, resume));
                self.switch_to(resume);
                // Back inside them, in this call's frame.
                for handler in self.guards.clone() {
                    let body = self.new_block();
                    self.terminate(Terminator::Guard { body, handler });
                    self.switch_to(body);
                }
                Operand::Const(Const::Void)
            }
            ExprKind::WorkerTake { ty: t, handle } => {
                let h = self.expr(handle);
                let t = self.mono(t);
                self.rvalue_to_temp(Rvalue::WorkerTake(t, h), ty, e.span)
            }
            ExprKind::ExitUncaught(inner) => {
                let v = self.expr(inner);
                let tag = Operand::Const(Const::Int(0, IntTy::U32));
                self.set_exception(v, tag, e.span);
                let (raw, tag) = self.take_exception(e.span);
                let (class, msg) = self.exception_text(&raw, &tag, e.span);
                self.diverge(Terminator::Uncaught { class, msg });
                Operand::Const(Const::Void)
            }
            ExprKind::ThrowUnchecked(inner) => {
                let v = self.expr(inner);
                let tag = Operand::Const(Const::Int(0, IntTy::U32));
                self.set_exception(v, tag, e.span);
                let target = self.raise_target();
                self.diverge(Terminator::Goto(target));
                Operand::Const(Const::Void)
            }
            ExprKind::Throw(inner) => {
                let v = self.expr(inner);
                let it = self.ty(inner);
                match self.cx.program.throw_tag(&it) {
                    // A lightweight throwable travels boxed, with its type's tag.
                    Some(k) => {
                        let boxed = self.rvalue_to_temp(
                            Rvalue::BoxNew(it.clone(), v),
                            Ty::Ref(Box::new(it)),
                            e.span,
                        );
                        let tag = Operand::Const(Const::Int(k as i128, IntTy::U32));
                        self.set_exception(boxed, tag, e.span);
                    }
                    None => {
                        let tag = Operand::Const(Const::Int(0, IntTy::U32));
                        self.set_exception(v, tag, e.span);
                    }
                }
                let target = self.raise_target();
                self.diverge(Terminator::Goto(target));
                Operand::Const(Const::Void)
            }
            ExprKind::Try {
                body,
                catches,
                finally,
            } => self.try_expr(body, catches, finally.as_deref(), ty, e.span),
            ExprKind::Break(value) => {
                let v = value.as_ref().map(|v| (self.expr(v), self.ty(v)));
                let v = match v {
                    Some((v, vt)) if !self.finallies.is_empty() && has_value(&vt) => {
                        let t = self.rvalue_to_temp(Rvalue::Use(v), vt.clone(), e.span);
                        Some((t, vt))
                    }
                    v => v,
                };
                self.run_finallies(self.loops.len());
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
                self.run_finallies(self.loops.len());
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
                let v = match v {
                    Some((v, vt)) if !self.finallies.is_empty() && has_value(&vt) => {
                        let t = self.rvalue_to_temp(Rvalue::Use(v), vt.clone(), e.span);
                        Some((t, vt))
                    }
                    v => v,
                };
                self.run_finallies(0);
                if self.generator.is_some() {
                    self.generator_done(e.span);
                    let dead = self.new_block();
                    self.switch_to(dead);
                    return Operand::Const(Const::Void);
                }
                if self.hir_fn.is_entry {
                    self.exit_hooks(e.span);
                }
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
                if matches!((&from, to), (Ty::Ref(_), Ty::Ref(_))) {
                    // A pointer to another pointee: the same address.
                    return self.rvalue_to_temp(Rvalue::Use(v), to.clone(), e.span);
                }
                self.rvalue_to_temp(Rvalue::Cast(v, to.clone()), to.clone(), e.span)
            }
        }
    }

    /// `for (var of iter) body` over a range, an array, a fixed array or a string.
    /// `for (x of it)` over an `Iterator<T>` (or a value's `iterator()`): call `next()` until
    /// it gives `null`.
    fn for_of_iterator(
        &mut self,
        var: hir::LocalId,
        iter: &Expr,
        body: &Expr,
        it_ty: Ty,
        span: Span,
    ) {
        let v = self.expr(iter);
        let (it, it_ty) = match self.cx.types.iterables.get(&iter.id).copied() {
            Some(m) => {
                let m = self.method_of(m, &it_ty);
                let iter_ty = self.iterator_ty_of(&it_ty);
                let recv = match &it_ty {
                    Ty::Class(..) => v,
                    _ => {
                        let tmp = Place::local(self.temp(it_ty.clone()));
                        self.assign(tmp.clone(), Rvalue::Use(v), span);
                        self.rvalue_to_temp(
                            Rvalue::Ref(tmp),
                            Ty::Ref(Box::new(it_ty.clone())),
                            span,
                        )
                    }
                };
                (
                    self.call_fn_value(m, vec![recv], iter_ty.clone(), span),
                    iter_ty,
                )
            }
            None => (v, it_ty),
        };
        let it_place = Place::local(self.temp(it_ty.clone()));
        self.assign(it_place.clone(), Rvalue::Use(it), span);
        let sid = self.cx.program.iterator_class.expect("prelude");
        let next = self
            .cx
            .program
            .struct_def(sid)
            .method("next")
            .expect("Iterator.next");
        let next = self.method_of(next, &it_ty);
        let elem = it_ty.type_args().first().cloned().unwrap_or(Ty::Void);
        let opt = elem.clone().nullable();
        let cond_bb = self.new_block();
        let body_bb = self.new_block();
        let exit = self.new_block();
        self.terminate(Terminator::Goto(cond_bb));
        self.switch_to(cond_bb);
        let item = self.call_fn_value(next, vec![Operand::Copy(it_place)], opt.clone(), span);
        let item_place = Place::local(self.temp(opt));
        self.assign(item_place.clone(), Rvalue::Use(item), span);
        let done = self.rvalue_to_temp(
            Rvalue::IsNull(Operand::Copy(item_place.clone())),
            Ty::Bool,
            span,
        );
        self.terminate(Terminator::Branch {
            cond: done,
            then: exit,
            else_: body_bb,
        });
        self.switch_to(body_bb);
        if has_value(&self.var_ty(hir::Var::Local(var))) {
            let var_place = self.place(hir::Var::Local(var));
            self.assign(var_place, Rvalue::Unwrap(Operand::Copy(item_place)), span);
        }
        self.loops.push(LoopTarget {
            brk: exit,
            cont: cond_bb,
            result: None,
        });
        self.expr(body);
        self.loops.pop();
        self.terminate(Terminator::Goto(cond_bb));
        self.switch_to(exit);
    }

    /// The `Iterator<T>` type an iterable's `iterator()` returns.
    fn iterator_ty_of(&self, t: &Ty) -> Ty {
        let (sid, args) = match t {
            Ty::Class(s, _, a) | Ty::Struct(s, _, a) => (*s, a.clone()),
            _ => return Ty::Error,
        };
        let m = self
            .cx
            .program
            .struct_def(sid)
            .method("iterator")
            .expect("checked");
        self.cx.types.sigs[m.0 as usize]
            .ret
            .subst(Ty::type_owner(sid), &args)
    }

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
        let iterator = self.cx.program.iterator_class;
        let is_iterator = matches!(&it_ty, Ty::Class(s, ..) if Some(*s) == iterator);
        if is_iterator || self.cx.types.iterables.contains_key(&iter.id) {
            self.for_of_iterator(var, iter, body, it_ty, span);
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
            "slice" => {
                let s = self.expr(object);
                let a = self.expr(&args[0]);
                let b = match args.get(1) {
                    Some(x) => self.expr(x),
                    None => Operand::Const(Const::Int(i64::MAX as i128, IntTy::I64)),
                };
                self.rvalue_to_temp(Rvalue::StrSlice(s, a, b), Ty::Str, e.span)
            }
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
        use ast::BinOp as B;
        if matches!(self.ty_of(l), Ty::Ref(_)) && matches!(op, B::Add | B::Sub) {
            let p = self.expr(l);
            let n = self.expr(r);
            return self.rvalue_to_temp(Rvalue::PtrOffset(p, n, op == B::Sub), ty, e.span);
        }
        // In generic code (`a == b` with `a: K`), an instance whose type is a class writing
        // `equals` / `compareTo` uses them, as the concrete code would.
        let generic_class_op = match (self.ty_of(l), op) {
            (Ty::Class(sid, _, args), B::Eq | B::Ne | B::Lt | B::Le | B::Gt | B::Ge)
                if args.is_empty() && !self.cx.types.op_calls.contains_key(&e.id) =>
            {
                let name = if matches!(op, B::Eq | B::Ne) {
                    "equals"
                } else {
                    "compareTo"
                };
                self.cx.program.struct_def(*sid).method(name)
            }
            _ => None,
        };
        if let Some(m) = self
            .cx
            .types
            .op_calls
            .get(&e.id)
            .copied()
            .or(generic_class_op)
        {
            // A user operator, or `equals` / `compareTo`.
            let rt = self.cx.types.sigs[m.0 as usize].ret.clone();
            let v = self.user_call(e, m, l, &[r], rt);
            return match op {
                B::Lt | B::Le | B::Gt | B::Ge => self.rvalue_to_temp(
                    Rvalue::Binary(bin_op(op), v, Operand::Const(Const::Int(0, IntTy::I32))),
                    Ty::Bool,
                    e.span,
                ),
                B::Ne => self.rvalue_to_temp(Rvalue::Unary(mir::UnOp::Not, v), Ty::Bool, e.span),
                _ => v,
            };
        }
        if self.cx.types.derived.get(&e.id) == Some(&ppl_sema::typeck::Derived::Equals) {
            let t = self.ty_of(l).clone();
            let a = self.expr(l);
            let b = self.expr(r);
            let eq = self.eq_values(a, b, &t, e.span);
            if op == B::Ne {
                return self.rvalue_to_temp(Rvalue::Unary(mir::UnOp::Not, eq), Ty::Bool, e.span);
            }
            return eq;
        }
        if self.cx.types.derived.get(&e.id) == Some(&ppl_sema::typeck::Derived::Compare) {
            let t = self.ty_of(l).clone();
            let a = self.expr(l);
            let b = self.expr(r);
            let c = self.compare_values(a, b, &t, e.span);
            return self.rvalue_to_temp(
                Rvalue::Binary(bin_op(op), c, Operand::Const(Const::Int(0, IntTy::I32))),
                Ty::Bool,
                e.span,
            );
        }
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
                let mut lv = self.expr(l);
                let mut rv = self.expr(r);
                if op == ast::BinOp::Add && ty.erase() == Ty::Str {
                    // `"n = " + n`: the other side's string form.
                    let lt = self.cx.types.final_ty(l.id).clone();
                    let rt = self.cx.types.final_ty(r.id).clone();
                    lv = self.stringify(lv, &lt, l.span);
                    rv = self.stringify(rv, &rt, r.span);
                }
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
        self.show_value(v, t, span, false)
    }

    /// The string form of a value; `quote`: strings and characters as literals (`"a"`, `'a'`)
    /// and compound values in the debug format, as fields in a class's default form show.
    fn show_value(&mut self, v: Operand, t: &Ty, span: Span, quote: bool) -> Operand {
        let debug = |b: &mut Self, v: Operand| {
            let spec = ppl_sema::format::parse("?").expect("valid spec");
            b.rvalue_to_temp(Rvalue::Format(v, spec), Ty::Str, span)
        };
        if *t == Ty::Str {
            return if quote { debug(self, v) } else { v };
        }
        if let Ty::Class(..) = t {
            let f = self.cx.show_fn(t);
            let dest = Place::local(self.temp(Ty::Str));
            let next = self.new_block();
            let loc = self.loc(span);
            self.terminate(Terminator::Call {
                callee: Callee::Fn(f),
                args: vec![v],
                dest: Some(dest.clone()),
                next,
                loc,
            });
            self.switch_to(next);
            return Operand::Copy(dest);
        }
        if let Ty::Immutable(inner) | Ty::Distinct(_, _, inner) = t {
            return self.show_value(v, inner, span, quote);
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
                let s = self.show_value(part, &m.erase(), span, quote);
                self.assign(res.clone(), Rvalue::Use(s), span);
                self.terminate(Terminator::Goto(join));
                self.switch_to(no);
            }
            self.terminate(Terminator::Goto(join));
            self.switch_to(join);
            return Operand::Copy(res);
        }
        if let Ty::Lits(b, _) = t {
            return self.show_value(v, b, span, quote);
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
            let s = self.show_value(inner_v, inner, span, quote);
            self.assign(res.clone(), Rvalue::Use(s), span);
            self.terminate(Terminator::Goto(join));
            self.switch_to(join);
            return Operand::Copy(res);
        }
        let plain = matches!(
            t.erase(),
            Ty::Int(_) | Ty::Float(_) | Ty::Bool | Ty::Char | Ty::Str
        );
        if quote && !debuggable(self.cx.program, t, &mut Vec::new()) {
            // No debug format (functions, interface values, pointers): just what it is.
            let text = match t.erase() {
                Ty::Fn(..) => "<function>".to_string(),
                other => format!("<{other}>"),
            };
            return Operand::Const(Const::Str(text));
        }
        if !plain || (quote && t.erase() == Ty::Char) {
            // A compound value (or a type parameter's instance without a string form): the
            // debug format.
            return debug(self, v);
        }
        self.rvalue_to_temp(Rvalue::ToStr(v), Ty::Str, span)
    }

    /// Ends the current block with a panic, continuing in a fresh (unreachable) block.
    fn panic_with(&mut self, msg: String, span: Span) {
        let loc = self.loc(span);
        self.terminate(Terminator::Panic {
            msg: Operand::Const(Const::Str(msg)),
            loc,
        });
        let next = self.new_block();
        self.switch_to(next);
    }

    /// A new object of core class type `t` with the given fields set (by name).
    fn core_object(&mut self, t: &Ty, fields: Vec<(&str, Operand)>, span: Span) -> Operand {
        let obj = self.temp(t.clone());
        self.assign(Place::local(obj), Rvalue::New(t.clone()), span);
        let Ty::Class(sid, _, _) = t else {
            unreachable!("core object of {t}")
        };
        for (name, v) in fields {
            let (i, _) = self
                .cx
                .program
                .struct_def(*sid)
                .field(name)
                .expect("core field");
            self.assign(Place::local(obj).field(i as u32), Rvalue::Use(v), span);
        }
        Operand::Copy(Place::local(obj))
    }

    /// An array of the operands.
    fn array_of(&mut self, elem: &Ty, items: Vec<Operand>, span: Span) -> Operand {
        self.rvalue_to_temp(
            Rvalue::ArrayNew(elem.clone(), items),
            Ty::Array(Box::new(elem.clone())),
            span,
        )
    }

    /// A `__Type` for `t`.
    fn reflected_type(&mut self, name: String, kind: &str, span: Span) -> Operand {
        let sid = self.cx.core_struct("__Type");
        let t = self.cx.program.struct_ty(sid);
        let s = |v: String| Operand::Const(Const::Str(v));
        self.core_object(&t, vec![("name", s(name)), ("kind", s(kind.into()))], span)
    }

    /// The body of a type-info function (`Synth::Info`): with the value `v` of a non-generic
    /// class type, first the subclasses' descriptions for their instances; then a new
    /// `__TypeInfo<seen>` describing `of`.
    fn info_body(
        &mut self,
        seen: &Ty,
        of: &Ty,
        file: ppl_diag::FileId,
        v: Option<Local>,
        span: Span,
    ) -> Operand {
        let info_ty = self.cx.type_info_ty(seen);
        let res = Place::local(self.temp(info_ty.clone()));
        let done = self.new_block();
        if let (Some(v), Ty::Class(sid, _, args)) = (v, of)
            && args.is_empty()
        {
            let direct: Vec<StructId> = subclasses(self.cx.program, *sid)
                .into_iter()
                .filter(|s| self.cx.program.struct_def(*s).base == Some(*sid))
                .collect();
            for d in direct {
                let is = self.rvalue_to_temp(
                    Rvalue::InstanceOf(Operand::Copy(Place::local(v)), d),
                    Ty::Bool,
                    span,
                );
                let (yes, no) = (self.new_block(), self.new_block());
                self.terminate(Terminator::Branch {
                    cond: is,
                    then: yes,
                    else_: no,
                });
                self.switch_to(yes);
                let f = self.cx.synth(Synth::Info {
                    seen: seen.clone(),
                    of: self.cx.program.struct_ty(d),
                    file,
                    dynamic: true,
                });
                self.call_into(f, vec![Operand::Copy(Place::local(v))], res.clone(), span);
                self.terminate(Terminator::Goto(done));
                self.switch_to(no);
            }
        }
        let info = self.describe(seen, of, file, span);
        self.assign(res.clone(), Rvalue::Use(info), span);
        self.terminate(Terminator::Goto(done));
        self.switch_to(done);
        Operand::Copy(res)
    }

    /// Calls `f` with `args`, storing the result in `dest`.
    fn call_into(&mut self, f: FnId, args: Vec<Operand>, dest: Place, span: Span) {
        let next = self.new_block();
        let loc = self.loc(span);
        self.terminate(Terminator::Call {
            callee: Callee::Fn(f),
            args,
            dest: Some(dest),
            next,
            loc,
        });
        self.switch_to(next);
    }

    /// A new `__TypeInfo<seen>` describing type `of` (its base class's description included).
    fn describe(&mut self, seen: &Ty, of: &Ty, file: ppl_diag::FileId, span: Span) -> Operand {
        let s = |v: String| Operand::Const(Const::Str(v));
        let targs = std::slice::from_ref(seen);
        let (_, fields_ty) = self.cx.core_field_ty("__TypeInfo", "fields", targs);
        let (_, methods_ty) = self.cx.core_field_ty("__TypeInfo", "methods", targs);
        let (_, ifaces_ty) = self.cx.core_field_ty("__TypeInfo", "interfaces", targs);
        let (_, decs_ty) = self.cx.core_field_ty("__TypeInfo", "decorators", targs);
        let elem = |t: &Ty| match t {
            Ty::Array(e) => (**e).clone(),
            other => unreachable!("array field of {other}"),
        };
        let (field_ty, method_ty, dec_ty) = (elem(&fields_ty), elem(&methods_ty), elem(&decs_ty));
        let def = match of {
            Ty::Struct(sid, _, args) | Ty::Class(sid, _, args) | Ty::Iface(sid, _, args) => {
                Some((*sid, args.clone()))
            }
            _ => None,
        };
        // Private members only in the declaring module (spec §11.3).
        let visible = |n: &str, d: &hir::StructDef| {
            !n.starts_with("__") && (!d.private.contains(n) || d.name_span.file == file)
        };
        let mut fields = Vec::new();
        let mut methods = Vec::new();
        let mut ifaces = Vec::new();
        let mut decorators = Vec::new();
        let mut base = Operand::Const(Const::Null(Ty::Nullable(Box::new(
            self.cx.type_info_ty(seen),
        ))));
        if let Some((sid, args)) = def {
            let program = self.cx.program;
            let d = program.struct_def(sid);
            let owner = Ty::type_owner(sid);
            for (i, f) in d.fields.iter().enumerate() {
                if !visible(&f.name, d) {
                    continue;
                }
                let fty = f.ty.subst(owner, &args);
                let ty = self.reflected_type(fty.to_string(), kind_name(&fty), span);
                let decs = self.array_of(&dec_ty, Vec::new(), span);
                let id = Operand::Const(Const::Int(field_id(program, sid, i) as i128, IntTy::I64));
                let fo = self.core_object(
                    &field_ty,
                    vec![
                        ("name", s(f.name.clone())),
                        ("type", ty),
                        ("decorators", decs),
                        ("isReadonly", Operand::Const(Const::Bool(f.readonly))),
                        ("id", id),
                    ],
                    span,
                );
                fields.push(fo);
            }
            for (n, _) in &d.methods {
                if visible(n, d) {
                    let m = self.rvalue_to_temp(
                        Rvalue::Aggregate(method_ty.clone(), vec![s(n.clone())]),
                        method_ty.clone(),
                        span,
                    );
                    methods.push(m);
                }
            }
            for i in d.implements.clone() {
                let name = program.struct_def(i).name.clone();
                ifaces.push(self.reflected_type(name, "interface", span));
            }
            for (name, dargs) in d.decorators.clone() {
                let items = dargs.into_iter().map(s).collect();
                let a = self.array_of(&Ty::Str, items, span);
                decorators.push(self.core_object(
                    &dec_ty,
                    vec![("name", s(name)), ("args", a)],
                    span,
                ));
            }
            if let Some(b) = d.base {
                let bt = program.struct_ty(b);
                if program.struct_def(b).generics.is_empty() {
                    let f = self.cx.synth(Synth::Info {
                        seen: seen.clone(),
                        of: bt,
                        file,
                        dynamic: false,
                    });
                    let info_ty = self.cx.type_info_ty(seen);
                    let r = Place::local(self.temp(info_ty.clone()));
                    self.call_into(f, Vec::new(), r.clone(), span);
                    base = self.rvalue_to_temp(
                        Rvalue::WrapSome(Operand::Copy(r), Ty::Nullable(Box::new(info_ty.clone()))),
                        Ty::Nullable(Box::new(info_ty)),
                        span,
                    );
                }
            }
        }
        let fields = self.array_of(&field_ty, fields, span);
        let methods = self.array_of(&method_ty, methods, span);
        let ifaces = self.array_of(&elem(&ifaces_ty), ifaces, span);
        let decorators = self.array_of(&dec_ty, decorators, span);
        let info_ty = self.cx.type_info_ty(seen);
        self.core_object(
            &info_ty,
            vec![
                ("name", s(of.to_string())),
                ("kind", s(kind_name(of).into())),
                ("fields", fields),
                ("methods", methods),
                ("interfaces", ifaces),
                ("base", base),
                ("decorators", decorators),
            ],
            span,
        )
    }

    /// The body of a field accessor (`Synth::Get`/`Set`/`FieldShow`) for objects of type `t`:
    /// finds field `fid` among `t`'s fields and those of its subclasses (checking the runtime
    /// class for a subclass's field), checks its type and reads, writes or shows it.
    fn access_body(
        &mut self,
        t: &Ty,
        access: Access,
        obj: Local,
        fid: Local,
        value: Option<Local>,
        span: Span,
    ) {
        let program = self.cx.program;
        // (the class or struct the field is in, its index, its type, its id, a downcast)
        let mut cands: Vec<(StructId, usize, Ty, i64, bool)> = Vec::new();
        let (sid, args) = match t {
            Ty::Struct(sid, _, args) | Ty::Class(sid, _, args) => (*sid, args.clone()),
            _ => {
                self.panic_with(format!("`{t}` has no fields"), span);
                self.terminate(Terminator::Unreachable);
                return;
            }
        };
        let owner = Ty::type_owner(sid);
        for (i, f) in program.struct_def(sid).fields.iter().enumerate() {
            cands.push((
                sid,
                i,
                f.ty.subst(owner, &args),
                field_id(program, sid, i),
                false,
            ));
        }
        if matches!(t, Ty::Class(..)) && args.is_empty() {
            let own = program.struct_def(sid).fields.len();
            for s in subclasses(program, sid) {
                for (i, f) in program.struct_def(s).fields.iter().enumerate().skip(own) {
                    let id = field_id(program, s, i);
                    if cands.iter().all(|c| c.3 != id) {
                        cands.push((StructId((id >> 20) as u32), i, f.ty.clone(), id, true));
                    }
                }
            }
        }
        let is_struct = matches!(t, Ty::Struct(..));
        for (k, i, fty, id, downcast) in cands {
            let eq = self.rvalue_to_temp(
                Rvalue::Binary(
                    mir::BinOp::Eq,
                    Operand::Copy(Place::local(fid)),
                    Operand::Const(Const::Int(id as i128, IntTy::I64)),
                ),
                Ty::Bool,
                span,
            );
            let (yes, no) = (self.new_block(), self.new_block());
            self.terminate(Terminator::Branch {
                cond: eq,
                then: yes,
                else_: no,
            });
            self.switch_to(yes);
            let kd = program.struct_def(k);
            let fname = kd.fields[i].name.clone();
            let place = if downcast {
                let ok = self.rvalue_to_temp(
                    Rvalue::InstanceOf(Operand::Copy(Place::local(obj)), k),
                    Ty::Bool,
                    span,
                );
                let (fine, bad) = (self.new_block(), self.new_block());
                self.terminate(Terminator::Branch {
                    cond: ok,
                    then: fine,
                    else_: bad,
                });
                self.switch_to(bad);
                self.panic_with(
                    format!(
                        "reflection: the object isn't a `{}` (field `{fname}`)",
                        kd.name
                    ),
                    span,
                );
                self.terminate(Terminator::Unreachable);
                self.switch_to(fine);
                let st = program.struct_ty(k);
                let o =
                    self.rvalue_to_temp(Rvalue::Use(Operand::Copy(Place::local(obj))), st, span);
                let Operand::Copy(p) = o else { unreachable!() };
                p
            } else {
                Place::local(obj)
            };
            let fplace = place.field(i as u32);
            match &access {
                Access::Show => {
                    let v =
                        self.rvalue_to_temp(Rvalue::Use(Operand::Copy(fplace)), fty.clone(), span);
                    let text = self.show_value(v, &fty, span, true);
                    self.terminate(Terminator::Return(Some(text)));
                }
                Access::Get(v) | Access::Set(v) if fty.erase() != v.erase() => {
                    self.panic_with(
                        format!("reflection: field `{fname}` is `{fty}`, not `{v}`"),
                        span,
                    );
                    self.terminate(Terminator::Unreachable);
                }
                Access::Get(_) => {
                    self.terminate(Terminator::Return(Some(Operand::Copy(fplace))));
                }
                Access::Set(_) if is_struct => {
                    self.panic_with(
                        format!("reflection: `set` needs a class instance (`{t}` is a struct)"),
                        span,
                    );
                    self.terminate(Terminator::Unreachable);
                }
                Access::Set(_) if kd.fields[i].readonly => {
                    self.panic_with(format!("reflection: field `{fname}` is readonly"), span);
                    self.terminate(Terminator::Unreachable);
                }
                Access::Set(_) => {
                    let v = Operand::Copy(Place::local(value.expect("a value")));
                    self.assign(fplace, Rvalue::Use(v), span);
                    self.terminate(Terminator::Return(None));
                }
            }
            self.switch_to(no);
        }
        self.panic_with(format!("reflection: not a field of `{t}`"), span);
        self.terminate(Terminator::Unreachable);
    }

    /// Calls method `name` of core class `class` (not virtual, doesn't throw).
    fn core_call(
        &mut self,
        class: &str,
        name: &str,
        args: Vec<Operand>,
        ret: Ty,
        span: Span,
    ) -> Operand {
        let sid = self.cx.core_struct(class);
        let m = self
            .cx
            .program
            .struct_def(sid)
            .method(name)
            .unwrap_or_else(|| panic!("core method `{class}.{name}`"));
        let dest = has_value(&ret).then(|| Place::local(self.temp(ret.clone())));
        let next = self.new_block();
        let loc = self.loc(span);
        self.terminate(Terminator::Call {
            callee: Callee::Fn(m),
            args,
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

    /// The body of `Synth::JsonRead(t)`: reads a `t` with reader `r` (see `__JsonReader`;
    /// nested values are read by their own functions).
    fn json_read(&mut self, r: Local, t: &Ty, span: Span) -> Operand {
        let rd = || Operand::Copy(Place::local(r));
        let s = |v: String| Operand::Const(Const::Str(v));
        let tn = s(t.to_string());
        const R: &str = "__JsonReader";
        match t {
            Ty::Int(it @ (IntTy::U64 | IntTy::Usize)) => {
                let v = self.core_call(R, "readU64", vec![rd(), tn], Ty::Int(IntTy::U64), span);
                if *it == IntTy::U64 {
                    v
                } else {
                    self.rvalue_to_temp(Rvalue::Cast(v, t.clone()), t.clone(), span)
                }
            }
            Ty::Int(it) => {
                let neg = s(it.min().unsigned_abs().to_string());
                let pos = s(it.max().to_string());
                let v = self.core_call(R, "readInt", vec![rd(), tn, neg, pos], Ty::INT, span);
                if *it == IntTy::I64 {
                    v
                } else {
                    self.rvalue_to_temp(Rvalue::Cast(v, t.clone()), t.clone(), span)
                }
            }
            Ty::Float(ft) => {
                let f64t = Ty::Float(ppl_sema::ty::FloatTy::F64);
                let v = self.core_call(R, "readF64", vec![rd(), tn], f64t.clone(), span);
                if *t == f64t {
                    v
                } else {
                    let _ = ft;
                    self.rvalue_to_temp(Rvalue::Cast(v, t.clone()), t.clone(), span)
                }
            }
            Ty::Bool => self.core_call(R, "readBool", vec![rd(), tn], Ty::Bool, span),
            Ty::Str => self.core_call(R, "readString", vec![rd(), tn], Ty::Str, span),
            Ty::Char => self.core_call(R, "readChar", vec![rd(), tn], Ty::Char, span),
            Ty::Nullable(inner) => {
                let res = Place::local(self.temp(t.clone()));
                let is_null = self.core_call(R, "isNull", vec![rd()], Ty::Bool, span);
                let (null_bb, some_bb, done) =
                    (self.new_block(), self.new_block(), self.new_block());
                self.terminate(Terminator::Branch {
                    cond: is_null,
                    then: null_bb,
                    else_: some_bb,
                });
                self.switch_to(null_bb);
                self.assign(
                    res.clone(),
                    Rvalue::Use(Operand::Const(Const::Null(t.clone()))),
                    span,
                );
                self.terminate(Terminator::Goto(done));
                self.switch_to(some_bb);
                let f = self.cx.synth(Synth::JsonRead((**inner).clone()));
                let v = Place::local(self.temp((**inner).clone()));
                self.call_into(f, vec![rd()], v.clone(), span);
                self.assign(
                    res.clone(),
                    Rvalue::WrapSome(Operand::Copy(v), t.clone()),
                    span,
                );
                self.terminate(Terminator::Goto(done));
                self.switch_to(done);
                Operand::Copy(res)
            }
            Ty::Array(e) => {
                let arr = self.rvalue_to_temp(
                    Rvalue::ArrayNew((**e).clone(), Vec::new()),
                    t.clone(),
                    span,
                );
                self.core_call(R, "beginArray", vec![rd(), tn], Ty::Bool, span);
                let i = self.temp(Ty::INT);
                self.assign(
                    Place::local(i),
                    Rvalue::Use(Operand::Const(Const::Int(0, IntTy::I64))),
                    span,
                );
                let (head, body, done) = (self.new_block(), self.new_block(), self.new_block());
                self.terminate(Terminator::Goto(head));
                self.switch_to(head);
                let more = self.core_call(R, "more", vec![rd()], Ty::Bool, span);
                self.terminate(Terminator::Branch {
                    cond: more,
                    then: body,
                    else_: done,
                });
                self.switch_to(body);
                self.core_call(
                    R,
                    "pushIndex",
                    vec![rd(), Operand::Copy(Place::local(i))],
                    Ty::Void,
                    span,
                );
                let f = self.cx.synth(Synth::JsonRead((**e).clone()));
                let v = Place::local(self.temp((**e).clone()));
                self.call_into(f, vec![rd()], v.clone(), span);
                self.core_call(R, "pop", vec![rd()], Ty::Void, span);
                let unit = self.temp(Ty::Void);
                self.assign(
                    Place::local(unit),
                    Rvalue::ArrayPush(arr.clone(), Operand::Copy(v)),
                    span,
                );
                self.assign(
                    Place::local(i),
                    Rvalue::Binary(
                        mir::BinOp::Add,
                        Operand::Copy(Place::local(i)),
                        Operand::Const(Const::Int(1, IntTy::I64)),
                    ),
                    span,
                );
                self.terminate(Terminator::Goto(head));
                self.switch_to(done);
                arr
            }
            Ty::Struct(sid, _, args) | Ty::Class(sid, _, args) => {
                let program = self.cx.program;
                let def = program.struct_def(*sid);
                let owner = Ty::type_owner(*sid);
                let fields: Vec<(String, String, Ty)> = def
                    .fields
                    .iter()
                    .map(|f| (f.name.clone(), json_key(f), f.ty.subst(owner, args)))
                    .collect();
                // A value and a "seen" flag per field.
                let mut slots = Vec::new();
                for (_, _, fty) in &fields {
                    let v = self.temp(fty.clone());
                    let init = match fty {
                        Ty::Nullable(_) => Const::Null(fty.clone()),
                        _ => Const::Undef(fty.clone()),
                    };
                    self.assign(Place::local(v), Rvalue::Use(Operand::Const(init)), span);
                    let seen = self.temp(Ty::Bool);
                    self.assign(
                        Place::local(seen),
                        Rvalue::Use(Operand::Const(Const::Bool(false))),
                        span,
                    );
                    slots.push((v, seen));
                }
                self.core_call(R, "beginObject", vec![rd(), tn], Ty::Bool, span);
                let (head, body, done) = (self.new_block(), self.new_block(), self.new_block());
                self.terminate(Terminator::Goto(head));
                self.switch_to(head);
                let key_ty = Ty::Str.nullable();
                let key = self.core_call(R, "nextKey", vec![rd()], key_ty, span);
                let end = self.rvalue_to_temp(Rvalue::IsNull(key.clone()), Ty::Bool, span);
                self.terminate(Terminator::Branch {
                    cond: end,
                    then: done,
                    else_: body,
                });
                self.switch_to(body);
                let k = self.rvalue_to_temp(Rvalue::Unwrap(key), Ty::Str, span);
                for ((_, jkey, fty), (v, seen)) in fields.iter().zip(&slots) {
                    let eq = self.rvalue_to_temp(
                        Rvalue::Binary(mir::BinOp::Eq, k.clone(), s(jkey.clone())),
                        Ty::Bool,
                        span,
                    );
                    let (yes, no) = (self.new_block(), self.new_block());
                    self.terminate(Terminator::Branch {
                        cond: eq,
                        then: yes,
                        else_: no,
                    });
                    self.switch_to(yes);
                    self.core_call(R, "pushField", vec![rd(), s(jkey.clone())], Ty::Void, span);
                    let f = self.cx.synth(Synth::JsonRead(fty.clone()));
                    self.call_into(f, vec![rd()], Place::local(*v), span);
                    self.core_call(R, "pop", vec![rd()], Ty::Void, span);
                    self.assign(
                        Place::local(*seen),
                        Rvalue::Use(Operand::Const(Const::Bool(true))),
                        span,
                    );
                    self.terminate(Terminator::Goto(head));
                    self.switch_to(no);
                }
                // An unknown key.
                self.core_call(R, "skipValue", vec![rd()], Ty::Void, span);
                self.terminate(Terminator::Goto(head));
                self.switch_to(done);
                // Missing fields (a nullable one is `null`).
                for ((_, jkey, fty), (_, seen)) in fields.iter().zip(&slots) {
                    if matches!(fty, Ty::Nullable(_)) {
                        continue;
                    }
                    let (miss, fine) = (self.new_block(), self.new_block());
                    self.terminate(Terminator::Branch {
                        cond: Operand::Copy(Place::local(*seen)),
                        then: fine,
                        else_: miss,
                    });
                    self.switch_to(miss);
                    self.core_call(R, "missing", vec![rd(), s(jkey.clone())], Ty::Void, span);
                    self.terminate(Terminator::Goto(fine));
                    self.switch_to(fine);
                }
                let vals: Vec<Operand> = slots
                    .iter()
                    .map(|(v, _)| Operand::Copy(Place::local(*v)))
                    .collect();
                if def.is_class {
                    let obj = self.temp(t.clone());
                    self.assign(Place::local(obj), Rvalue::New(t.clone()), span);
                    for (i, v) in vals.into_iter().enumerate() {
                        self.assign(Place::local(obj).field(i as u32), Rvalue::Use(v), span);
                    }
                    Operand::Copy(Place::local(obj))
                } else {
                    self.rvalue_to_temp(Rvalue::Aggregate(t.clone(), vals), t.clone(), span)
                }
            }
            _ => {
                self.terminate(Terminator::Unreachable);
                let next = self.new_block();
                self.switch_to(next);
                Operand::Const(Const::Undef(t.clone()))
            }
        }
    }

    /// The body of `Synth::JsonWrite(t)`: writes value `v` with writer `w`.
    fn json_write(&mut self, w: Local, v: Local, t: &Ty, span: Span) {
        let wr = || Operand::Copy(Place::local(w));
        let val = || Operand::Copy(Place::local(v));
        let raw = |b: &mut Self, text: Operand| {
            b.core_call("__JsonWriter", "raw", vec![wr(), text], Ty::Void, span);
        };
        let lit = |text: &str| Operand::Const(Const::Str(text.to_string()));
        match t {
            Ty::Int(_) | Ty::Bool => {
                let text = self.rvalue_to_temp(Rvalue::ToStr(val()), Ty::Str, span);
                raw(self, text);
            }
            Ty::Float(_) => {
                let f64t = Ty::Float(ppl_sema::ty::FloatTy::F64);
                let x = if *t == f64t {
                    val()
                } else {
                    self.rvalue_to_temp(Rvalue::Cast(val(), f64t.clone()), f64t, span)
                };
                self.core_call("__JsonWriter", "num", vec![wr(), x], Ty::Void, span);
            }
            Ty::Str | Ty::Char => {
                let text = self.stringify(val(), t, span);
                self.core_call("__JsonWriter", "str", vec![wr(), text], Ty::Void, span);
            }
            Ty::Nullable(inner) => {
                let is_null = self.rvalue_to_temp(Rvalue::IsNull(val()), Ty::Bool, span);
                let (null_bb, some_bb, done) =
                    (self.new_block(), self.new_block(), self.new_block());
                self.terminate(Terminator::Branch {
                    cond: is_null,
                    then: null_bb,
                    else_: some_bb,
                });
                self.switch_to(null_bb);
                raw(self, lit("null"));
                self.terminate(Terminator::Goto(done));
                self.switch_to(some_bb);
                let x = self.rvalue_to_temp(Rvalue::Unwrap(val()), (**inner).clone(), span);
                let f = self.cx.synth(Synth::JsonWrite((**inner).clone()));
                self.json_call(f, wr(), x, span);
                self.terminate(Terminator::Goto(done));
                self.switch_to(done);
            }
            Ty::Array(e) => {
                raw(self, lit("["));
                let n = self.rvalue_to_temp(Rvalue::ArrayLen(val()), Ty::INT, span);
                let i = self.temp(Ty::INT);
                self.assign(
                    Place::local(i),
                    Rvalue::Use(Operand::Const(Const::Int(0, IntTy::I64))),
                    span,
                );
                let (head, body, done) = (self.new_block(), self.new_block(), self.new_block());
                self.terminate(Terminator::Goto(head));
                self.switch_to(head);
                let more = self.rvalue_to_temp(
                    Rvalue::Binary(mir::BinOp::Lt, Operand::Copy(Place::local(i)), n),
                    Ty::Bool,
                    span,
                );
                self.terminate(Terminator::Branch {
                    cond: more,
                    then: body,
                    else_: done,
                });
                self.switch_to(body);
                let first = self.rvalue_to_temp(
                    Rvalue::Binary(
                        mir::BinOp::Eq,
                        Operand::Copy(Place::local(i)),
                        Operand::Const(Const::Int(0, IntTy::I64)),
                    ),
                    Ty::Bool,
                    span,
                );
                let (comma, go) = (self.new_block(), self.new_block());
                self.terminate(Terminator::Branch {
                    cond: first,
                    then: go,
                    else_: comma,
                });
                self.switch_to(comma);
                raw(self, lit(","));
                self.terminate(Terminator::Goto(go));
                self.switch_to(go);
                let x = self.rvalue_to_temp(
                    Rvalue::Use(Operand::Copy(Place::local(v).index(i))),
                    (**e).clone(),
                    span,
                );
                let f = self.cx.synth(Synth::JsonWrite((**e).clone()));
                self.json_call(f, wr(), x, span);
                self.assign(
                    Place::local(i),
                    Rvalue::Binary(
                        mir::BinOp::Add,
                        Operand::Copy(Place::local(i)),
                        Operand::Const(Const::Int(1, IntTy::I64)),
                    ),
                    span,
                );
                self.terminate(Terminator::Goto(head));
                self.switch_to(done);
                raw(self, lit("]"));
            }
            Ty::Struct(sid, _, args) | Ty::Class(sid, _, args) => {
                let def = self.cx.program.struct_def(*sid);
                let owner = Ty::type_owner(*sid);
                let fields: Vec<(String, Ty)> = def
                    .fields
                    .iter()
                    .map(|f| (json_key(f), f.ty.subst(owner, args)))
                    .collect();
                for (i, (key, fty)) in fields.iter().enumerate() {
                    let sep = if i == 0 { "{" } else { "," };
                    raw(self, lit(&format!("{sep}{}:", json_quote(key))));
                    let x = self.rvalue_to_temp(
                        Rvalue::Use(Operand::Copy(Place::local(v).field(i as u32))),
                        fty.clone(),
                        span,
                    );
                    let f = self.cx.synth(Synth::JsonWrite(fty.clone()));
                    self.json_call(f, wr(), x, span);
                }
                raw(self, lit(if fields.is_empty() { "{}" } else { "}" }));
            }
            _ => {}
        }
    }

    /// Calls JSON writer function `f` for value `x`.
    fn json_call(&mut self, f: FnId, w: Operand, x: Operand, span: Span) {
        let next = self.new_block();
        let loc = self.loc(span);
        self.terminate(Terminator::Call {
            callee: Callee::Fn(f),
            args: vec![w, x],
            dest: None,
            next,
            loc,
        });
        self.switch_to(next);
    }

    /// The body of class type `t`'s show function, `this` its parameter. A class without
    /// `toString` first passes an instance of a (non-generic) subclass on to the subclass's
    /// show function; then it's `Name (field = value, …)`, or `Name (…)` for an object that's
    /// already being shown further up (a cycle).
    fn show_body(&mut self, this: Local, t: &Ty, span: Span) -> Operand {
        let Ty::Class(sid, name, args) = t else {
            unreachable!("show body of {t}")
        };
        let obj = || Operand::Copy(Place::local(this));
        if let Some(m) = self.cx.to_string_method(*sid) {
            let def = self.cx.program.struct_def(*sid);
            let callee = match def.virtual_slot("toString") {
                Some(slot) => Callee::Virtual { slot, decl: m },
                None => Callee::Fn(self.method_of(m, t)),
            };
            let dest = Place::local(self.temp(Ty::Str));
            let next = self.new_block();
            let loc = self.loc(span);
            self.terminate(Terminator::Call {
                callee,
                args: vec![obj()],
                dest: Some(dest.clone()),
                next,
                loc,
            });
            self.switch_to(next);
            return Operand::Copy(dest);
        }
        let res = Place::local(self.temp(Ty::Str));
        let done = self.new_block();
        if args.is_empty() {
            let subs: Vec<StructId> = (0..self.cx.program.structs.len())
                .map(|i| StructId(i as u32))
                .filter(|s| {
                    let d = self.cx.program.struct_def(*s);
                    d.is_class && d.base == Some(*sid) && d.generics.is_empty()
                })
                .collect();
            for s in subs {
                let is = self.rvalue_to_temp(Rvalue::InstanceOf(obj(), s), Ty::Bool, span);
                let (yes, no) = (self.new_block(), self.new_block());
                self.terminate(Terminator::Branch {
                    cond: is,
                    then: yes,
                    else_: no,
                });
                self.switch_to(yes);
                let st = self.cx.program.struct_ty(s);
                let sub = self.rvalue_to_temp(Rvalue::Use(obj()), st.clone(), span);
                let text = self.show_value(sub, &st, span, false);
                self.assign(res.clone(), Rvalue::Use(text), span);
                self.terminate(Terminator::Goto(done));
                self.switch_to(no);
            }
        }
        let entered = self.rvalue_to_temp(Rvalue::ShowEnter(obj()), Ty::Bool, span);
        let (fresh, cycle) = (self.new_block(), self.new_block());
        self.terminate(Terminator::Branch {
            cond: entered,
            then: fresh,
            else_: cycle,
        });
        self.switch_to(cycle);
        self.assign(
            res.clone(),
            Rvalue::Use(Operand::Const(Const::Str(format!("{name} (…)")))),
            span,
        );
        self.terminate(Terminator::Goto(done));
        self.switch_to(fresh);
        let owner = Ty::type_owner(*sid);
        let fields: Vec<(String, Ty)> = self
            .cx
            .program
            .struct_def(*sid)
            .fields
            .iter()
            .map(|f| (f.name.clone(), f.ty.subst(owner, args)))
            .collect();
        let mut acc = Operand::Const(Const::Str(format!("{name} (")));
        let mut first = true;
        for (i, (fname, fty)) in fields.iter().enumerate() {
            if !has_value(fty) {
                continue;
            }
            let sep = if first { "" } else { ", " };
            first = false;
            let label = Operand::Const(Const::Str(format!("{sep}{fname} = ")));
            acc = self.rvalue_to_temp(Rvalue::Binary(mir::BinOp::Add, acc, label), Ty::Str, span);
            let fv = self.rvalue_to_temp(
                Rvalue::Use(Operand::Copy(Place::local(this).field(i as u32))),
                fty.clone(),
                span,
            );
            let text = self.show_value(fv, fty, span, true);
            acc = self.rvalue_to_temp(Rvalue::Binary(mir::BinOp::Add, acc, text), Ty::Str, span);
        }
        let close = Operand::Const(Const::Str(")".into()));
        acc = self.rvalue_to_temp(Rvalue::Binary(mir::BinOp::Add, acc, close), Ty::Str, span);
        self.assign(res.clone(), Rvalue::Use(acc), span);
        self.rvalue_to_temp(Rvalue::ShowLeave(obj()), Ty::Void, span);
        self.terminate(Terminator::Goto(done));
        self.switch_to(done);
        Operand::Copy(res)
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
                if let Some(m) = self.cx.types.formattables.get(&p.expr.id).copied() {
                    let s =
                        self.format_call(v, &t, m, p.spec.as_deref().unwrap_or(""), p.expr.span);
                    pieces.push(s);
                    continue;
                }
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

    /// `v.format(spec)` for a `Formattable` value of type `t` (virtual for classes).
    fn format_call(&mut self, v: Operand, t: &Ty, m: FnId, spec: &str, span: Span) -> Operand {
        let spec = Operand::Const(Const::Str(spec.to_string()));
        if let Ty::Class(sid, _, _) = t {
            let def = self.cx.program.struct_def(*sid);
            let callee = match def.virtual_slot("format") {
                Some(slot) => Callee::Virtual { slot, decl: m },
                None => Callee::Fn(self.method_of(m, t)),
            };
            let dest = Place::local(self.temp(Ty::Str));
            let next = self.new_block();
            let loc = self.loc(span);
            self.terminate(Terminator::Call {
                callee,
                args: vec![v, spec],
                dest: Some(dest.clone()),
                next,
                loc,
            });
            self.switch_to(next);
            return Operand::Copy(dest);
        }
        let m = self.method_of(m, t);
        let tmp = Place::local(self.temp(t.clone()));
        self.assign(tmp.clone(), Rvalue::Use(v), span);
        let recv = self.rvalue_to_temp(Rvalue::Ref(tmp), Ty::Ref(Box::new(t.clone())), span);
        self.call_fn_value(m, vec![recv, spec], Ty::Str, span)
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
    /// Method `m` of a value of type `recv` (an instance for a generic type).
    fn method_of(&mut self, m: FnId, recv: &Ty) -> FnId {
        let args = recv.type_args();
        match self.cx.program.func(m).owner {
            Some(sid) if !args.is_empty() => self
                .cx
                .instance(m, vec![(Ty::type_owner(sid), args.to_vec())]),
            _ => m,
        }
    }

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
        if primitive && name == "hashCode" {
            let a = self.expr(object);
            return self.rvalue_to_temp(Rvalue::Hash(a), Ty::Int(IntTy::U64), e.span);
        }
        if !primitive && !matches!(t, Ty::Struct(..) | Ty::Class(..) | Ty::Iface(..)) {
            // Enums, tuples, fixed arrays: the derived protocol.
            let d = match name {
                "compareTo" => ppl_sema::typeck::Derived::Compare,
                "hashCode" => ppl_sema::typeck::Derived::Hash,
                _ => ppl_sema::typeck::Derived::Equals,
            };
            return self.derived_call(d, object, args.first(), &t, e.span);
        }
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
        let Some(m) = def.method(name) else {
            // A derived protocol on the concrete type.
            let d = match name {
                "compareTo" => ppl_sema::typeck::Derived::Compare,
                "hashCode" => ppl_sema::typeck::Derived::Hash,
                _ => ppl_sema::typeck::Derived::Equals,
            };
            return self.derived_call(d, object, args.first(), &t, e.span);
        };
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

    /// The receiver argument for a method of the type of `place`'s value: a reference for a
    /// struct, the object for a class.
    fn receiver(&mut self, place: &Place, t: &Ty, span: Span) -> Operand {
        match t {
            Ty::Class(..) => Operand::Copy(place.clone()),
            _ => self.rvalue_to_temp(
                Rvalue::Ref(place.clone()),
                Ty::Ref(Box::new(t.clone())),
                span,
            ),
        }
    }

    /// Calls user method `m` on `object` with these arguments.
    fn user_call(&mut self, e: &Expr, m: FnId, object: &Expr, args: &[&Expr], ty: Ty) -> Operand {
        let t = self.ty_of(object).clone();
        let place = self.place_of(object);
        let recv = self.receiver(&place, &t, e.span);
        let mut ops = vec![recv];
        for a in args {
            let v = self.expr(a);
            ops.push(v);
        }
        let m = self.method_target(e, m);
        self.call_fn_value(m, ops, ty, e.span)
    }

    /// `a.equals(b)`, `a.compareTo(b)`, `a.hashCode()` derived field by field.
    fn derived_call(
        &mut self,
        d: ppl_sema::typeck::Derived,
        object: &Expr,
        arg: Option<&Expr>,
        t: &Ty,
        span: Span,
    ) -> Operand {
        use ppl_sema::typeck::Derived;
        let a = self.expr(object);
        match d {
            Derived::Hash => self.hash_value(a, t, span),
            Derived::Equals => {
                let b = self.expr(arg.expect("an argument"));
                self.eq_values(a, b, t, span)
            }
            Derived::Compare => {
                let b = self.expr(arg.expect("an argument"));
                self.compare_values(a, b, t, span)
            }
        }
    }

    /// `equals` derived field by field: `==` (class-typed parts compare their fields).
    fn eq_values(&mut self, a: Operand, b: Operand, t: &Ty, span: Span) -> Operand {
        let Ty::Class(sid, _, args) = t else {
            return self.rvalue_to_temp(Rvalue::Binary(mir::BinOp::Eq, a, b), Ty::Bool, span);
        };
        let def = self.cx.program.struct_def(*sid);
        let owner = Ty::type_owner(*sid);
        let fields: Vec<Ty> = def.fields.iter().map(|f| f.ty.subst(owner, args)).collect();
        let pa = self.in_place(a, t, span);
        let pb = self.in_place(b, t, span);
        let mut acc = Operand::Const(Const::Bool(true));
        for (i, ft) in fields.iter().enumerate() {
            let x = Operand::Copy(pa.field(i as u32));
            let y = Operand::Copy(pb.field(i as u32));
            let eq = self.eq_values(x, y, ft, span);
            acc = self.rvalue_to_temp(Rvalue::Binary(mir::BinOp::BitAnd, acc, eq), Ty::Bool, span);
        }
        acc
    }

    /// Stores an operand in a fresh place (so its parts can be projected).
    fn in_place(&mut self, v: Operand, t: &Ty, span: Span) -> Place {
        if let Operand::Copy(p) = &v {
            return p.clone();
        }
        let p = Place::local(self.temp(t.clone()));
        self.assign(p.clone(), Rvalue::Use(v), span);
        p
    }

    /// `compareTo` derived field by field: -1, 0 or 1 (`i32`).
    fn compare_values(&mut self, a: Operand, b: Operand, t: &Ty, span: Span) -> Operand {
        let res = Place::local(self.temp(Ty::Int(IntTy::I32)));
        self.assign(
            res.clone(),
            Rvalue::Use(Operand::Const(Const::Int(0, IntTy::I32))),
            span,
        );
        let done = self.new_block();
        let pa = self.in_place(a, t, span);
        let pb = self.in_place(b, t, span);
        self.cmp_into(&res, &pa, &pb, t, done, span);
        self.terminate(Terminator::Goto(done));
        self.switch_to(done);
        Operand::Copy(res)
    }

    /// Sets `res` and jumps to `done` if `a` and `b` differ in order; falls through if equal.
    fn cmp_into(&mut self, res: &Place, a: &Place, b: &Place, t: &Ty, done: BlockId, span: Span) {
        let set = |c: &mut Self, v: i128| {
            c.assign(
                res.clone(),
                Rvalue::Use(Operand::Const(Const::Int(v, IntTy::I32))),
                span,
            );
            c.terminate(Terminator::Goto(done));
        };
        let branch = |c: &mut Self, cond: Operand| -> BlockId {
            let yes = c.new_block();
            let no = c.new_block();
            c.terminate(Terminator::Branch {
                cond,
                then: yes,
                else_: no,
            });
            c.switch_to(yes);
            no
        };
        let te = t.erase();
        match &te {
            Ty::Int(_) | Ty::Float(_) | Ty::Char | Ty::Str => {
                let lt = self.rvalue_to_temp(
                    Rvalue::Binary(
                        mir::BinOp::Lt,
                        Operand::Copy(a.clone()),
                        Operand::Copy(b.clone()),
                    ),
                    Ty::Bool,
                    span,
                );
                let no = branch(self, lt);
                set(self, -1);
                self.switch_to(no);
                let gt = self.rvalue_to_temp(
                    Rvalue::Binary(
                        mir::BinOp::Gt,
                        Operand::Copy(a.clone()),
                        Operand::Copy(b.clone()),
                    ),
                    Ty::Bool,
                    span,
                );
                let no = branch(self, gt);
                set(self, 1);
                self.switch_to(no);
            }
            Ty::Bool => {
                // false < true
                let ne = self.rvalue_to_temp(
                    Rvalue::Binary(
                        mir::BinOp::Ne,
                        Operand::Copy(a.clone()),
                        Operand::Copy(b.clone()),
                    ),
                    Ty::Bool,
                    span,
                );
                let same = branch(self, ne);
                let a_false = branch(self, Operand::Copy(a.clone()));
                set(self, 1);
                self.switch_to(a_false);
                set(self, -1);
                self.switch_to(same);
            }
            Ty::Struct(sid, _, args) | Ty::Class(sid, _, args) => {
                let def = self.cx.program.struct_def(*sid);
                if let Some(m) = def.method("compareTo") {
                    let recv = self.receiver(a, &te, span);
                    let c = self.call_fn_value(
                        m,
                        vec![recv, Operand::Copy(b.clone())],
                        Ty::Int(IntTy::I32),
                        span,
                    );
                    let ne = self.rvalue_to_temp(
                        Rvalue::Binary(
                            mir::BinOp::Ne,
                            c.clone(),
                            Operand::Const(Const::Int(0, IntTy::I32)),
                        ),
                        Ty::Bool,
                        span,
                    );
                    let no = branch(self, ne);
                    self.assign(res.clone(), Rvalue::Use(c), span);
                    self.terminate(Terminator::Goto(done));
                    self.switch_to(no);
                    return;
                }
                let owner = Ty::type_owner(*sid);
                let fields: Vec<Ty> = def.fields.iter().map(|f| f.ty.subst(owner, args)).collect();
                for (i, ft) in fields.iter().enumerate() {
                    self.cmp_into(res, &a.field(i as u32), &b.field(i as u32), ft, done, span);
                }
            }
            Ty::Tuple(ts) => {
                for (i, ft) in ts.iter().enumerate() {
                    self.cmp_into(res, &a.field(i as u32), &b.field(i as u32), ft, done, span);
                }
            }
            Ty::Range(it) => {
                for i in 0..3 {
                    self.cmp_into(res, &a.field(i), &b.field(i), &Ty::Int(*it), done, span);
                }
            }
            Ty::Fixed(et, n) => {
                for i in 0..*n {
                    let k = self.const_index(i as i128, span);
                    self.cmp_into(res, &a.index(k), &b.index(k), et, done, span);
                }
            }
            Ty::Enum(eid, _) => {
                let u32t = Ty::Int(IntTy::U32);
                let ta = Place::local(self.temp(u32t.clone()));
                let tb = Place::local(self.temp(u32t.clone()));
                self.assign(ta.clone(), Rvalue::Tag(Operand::Copy(a.clone())), span);
                self.assign(tb.clone(), Rvalue::Tag(Operand::Copy(b.clone())), span);
                self.cmp_into(res, &ta, &tb, &u32t, done, span);
                let variants: Vec<Vec<Ty>> = self
                    .cx
                    .program
                    .enum_def(*eid)
                    .variants
                    .iter()
                    .map(|v| v.fields.iter().map(|f| f.ty.clone()).collect())
                    .collect();
                let after = self.new_block();
                for (vi, fields) in variants.iter().enumerate() {
                    if fields.is_empty() {
                        continue;
                    }
                    let is_v = self.rvalue_to_temp(
                        Rvalue::Binary(
                            mir::BinOp::Eq,
                            Operand::Copy(ta.clone()),
                            Operand::Const(Const::Int(vi as i128, IntTy::U32)),
                        ),
                        Ty::Bool,
                        span,
                    );
                    let no = branch(self, is_v);
                    for (i, ft) in fields.iter().enumerate() {
                        let (fa, fb) = (
                            a.variant_field(vi as u32, i as u32),
                            b.variant_field(vi as u32, i as u32),
                        );
                        self.cmp_into(res, &fa, &fb, ft, done, span);
                    }
                    self.terminate(Terminator::Goto(after));
                    self.switch_to(no);
                }
                self.terminate(Terminator::Goto(after));
                self.switch_to(after);
            }
            Ty::Nullable(inner) => {
                // null sorts first.
                let na =
                    self.rvalue_to_temp(Rvalue::IsNull(Operand::Copy(a.clone())), Ty::Bool, span);
                let nb =
                    self.rvalue_to_temp(Rvalue::IsNull(Operand::Copy(b.clone())), Ty::Bool, span);
                let a_some = branch(self, na);
                let both_null_no = branch(self, nb.clone());
                // a null, b null: equal
                self.terminate(Terminator::Goto(done));
                self.switch_to(both_null_no);
                set(self, -1);
                self.switch_to(a_some);
                let both_some = branch(self, nb);
                set(self, 1);
                self.switch_to(both_some);
                self.cmp_into(res, &a.payload(), &b.payload(), inner, done, span);
            }
            _ => {}
        }
    }

    /// `hashCode` derived field by field.
    fn hash_value(&mut self, v: Operand, t: &Ty, span: Span) -> Operand {
        let u64t = Ty::Int(IntTy::U64);
        let te = t.erase();
        match &te {
            Ty::Int(_) | Ty::Float(_) | Ty::Char | Ty::Str | Ty::Bool => {
                self.rvalue_to_temp(Rvalue::Hash(v), u64t, span)
            }
            Ty::Struct(sid, _, args) | Ty::Class(sid, _, args) => {
                let def = self.cx.program.struct_def(*sid);
                if let Some(m) = def.method("hashCode") {
                    let p = self.in_place(v, &te, span);
                    let recv = self.receiver(&p, &te, span);
                    return self.call_fn_value(m, vec![recv], u64t, span);
                }
                let owner = Ty::type_owner(*sid);
                let fields: Vec<Ty> = def.fields.iter().map(|f| f.ty.subst(owner, args)).collect();
                let p = self.in_place(v, &te, span);
                let parts: Vec<(Place, Ty)> = fields
                    .into_iter()
                    .enumerate()
                    .map(|(i, ft)| (p.field(i as u32), ft))
                    .collect();
                self.hash_parts(parts, span)
            }
            Ty::Tuple(ts) => {
                let p = self.in_place(v, &te, span);
                let parts = ts
                    .iter()
                    .enumerate()
                    .map(|(i, t)| (p.field(i as u32), t.clone()))
                    .collect();
                self.hash_parts(parts, span)
            }
            Ty::Fixed(et, n) => {
                let p = self.in_place(v, &te, span);
                let parts = (0..*n)
                    .map(|i| {
                        let k = self.const_index(i as i128, span);
                        (p.index(k), (**et).clone())
                    })
                    .collect();
                self.hash_parts(parts, span)
            }
            Ty::Enum(eid, _) => {
                let p = self.in_place(v, &te, span);
                let res = Place::local(self.temp(u64t.clone()));
                let tag = self.rvalue_to_temp(
                    Rvalue::Tag(Operand::Copy(p.clone())),
                    Ty::Int(IntTy::U32),
                    span,
                );
                let h = self.rvalue_to_temp(Rvalue::Hash(tag.clone()), u64t.clone(), span);
                self.assign(res.clone(), Rvalue::Use(h), span);
                let variants: Vec<Vec<Ty>> = self
                    .cx
                    .program
                    .enum_def(*eid)
                    .variants
                    .iter()
                    .map(|v| v.fields.iter().map(|f| f.ty.clone()).collect())
                    .collect();
                let after = self.new_block();
                for (vi, fields) in variants.iter().enumerate() {
                    if fields.is_empty() {
                        continue;
                    }
                    let yes = self.new_block();
                    let no = self.new_block();
                    let is_v = self.rvalue_to_temp(
                        Rvalue::Binary(
                            mir::BinOp::Eq,
                            tag.clone(),
                            Operand::Const(Const::Int(vi as i128, IntTy::U32)),
                        ),
                        Ty::Bool,
                        span,
                    );
                    self.terminate(Terminator::Branch {
                        cond: is_v,
                        then: yes,
                        else_: no,
                    });
                    self.switch_to(yes);
                    for (i, ft) in fields.iter().enumerate() {
                        let fh = self.hash_value(
                            Operand::Copy(p.variant_field(vi as u32, i as u32)),
                            ft,
                            span,
                        );
                        self.assign(
                            res.clone(),
                            Rvalue::HashCombine(Operand::Copy(res.clone()), fh),
                            span,
                        );
                    }
                    self.terminate(Terminator::Goto(after));
                    self.switch_to(no);
                }
                self.terminate(Terminator::Goto(after));
                self.switch_to(after);
                Operand::Copy(res)
            }
            Ty::Nullable(inner) => {
                let p = self.in_place(v, &te, span);
                let res = Place::local(self.temp(u64t.clone()));
                self.assign(
                    res.clone(),
                    Rvalue::Use(Operand::Const(Const::Int(0, IntTy::U64))),
                    span,
                );
                let n =
                    self.rvalue_to_temp(Rvalue::IsNull(Operand::Copy(p.clone())), Ty::Bool, span);
                let (some, join) = (self.new_block(), self.new_block());
                self.terminate(Terminator::Branch {
                    cond: n,
                    then: join,
                    else_: some,
                });
                self.switch_to(some);
                let h = self.hash_value(Operand::Copy(p.payload()), inner, span);
                self.assign(res.clone(), Rvalue::Use(h), span);
                self.terminate(Terminator::Goto(join));
                self.switch_to(join);
                Operand::Copy(res)
            }
            _ => Operand::Const(Const::Int(0, IntTy::U64)),
        }
    }

    fn hash_parts(&mut self, parts: Vec<(Place, Ty)>, span: Span) -> Operand {
        let u64t = Ty::Int(IntTy::U64);
        let acc = Place::local(self.temp(u64t.clone()));
        self.assign(
            acc.clone(),
            Rvalue::Use(Operand::Const(Const::Int(
                0xcbf2_9ce4_8422_2325,
                IntTy::U64,
            ))),
            span,
        );
        for (p, t) in parts {
            let h = self.hash_value(Operand::Copy(p), &t, span);
            self.assign(
                acc.clone(),
                Rvalue::HashCombine(Operand::Copy(acc.clone()), h),
                span,
            );
        }
        Operand::Copy(acc)
    }

    /// Calls function `id` with already-evaluated arguments.
    /// The end of the entry module's code: the core library's `__runEventLoop` (timers,
    /// microtasks and promises, then the due finalization callbacks; spec §10.1, §9.3).
    fn exit_hooks(&mut self, span: Span) {
        if self.cx.inline_entry
            && let Some(i) = self
                .cx
                .program
                .fns
                .iter()
                .position(|f| f.name == "__inlineWorkerMain" && f.span.file != self.cx.file_id)
        {
            self.call_fn_value(FnId(i as u32), Vec::new(), Ty::Void, span);
        }
        let hook = self
            .cx
            .program
            .fns
            .iter()
            .position(|f| f.name == "__runEventLoop" && f.span.file != self.cx.file_id);
        if let Some(i) = hook {
            self.call_fn_value(FnId(i as u32), Vec::new(), Ty::Void, span);
        }
    }

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
        if let (Some(id), ExprKind::Member { object, .. }) =
            (self.cx.types.extensions.get(&e.id).copied(), &callee.kind)
        {
            // A ppl extension of a built-in type: the receiver is the first argument (and
            // defaulted parameters get their flags, as in any call).
            let mut ops = vec![self.expr(object)];
            ops.extend(args.iter().map(|a| self.expr(a)));
            let given = ops.len();
            let sig = self.cx.types.sigs[id.0 as usize].clone();
            let defaults: Vec<bool> = self
                .cx
                .program
                .func(id)
                .defaults
                .iter()
                .map(Option::is_some)
                .collect();
            for t in sig.params.iter().skip(given) {
                ops.push(Operand::Const(Const::Undef(t.clone())));
            }
            for (k, d) in defaults.iter().enumerate() {
                if *d {
                    ops.push(Operand::Const(Const::Bool(k < given)));
                }
            }
            let id = match self.cx.types.instances.get(&e.id).cloned() {
                Some(targs) => self.cx.instance(id, vec![(id.0, targs)]),
                None => id,
            };
            return self.call_fn_value(id, ops, ty, e.span);
        }
        match &callee.kind {
            ExprKind::Member { object, .. } if self.cx.types.derived.contains_key(&e.id) => {
                let d = self.cx.types.derived[&e.id];
                let t = self.ty_of(object).clone();
                self.derived_call(d, object, args.first(), &t, e.span)
            }
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
                if matches!(self.ty_of(object), Ty::Ref(_))
                    && (name == "read" || name == "write") =>
            {
                let p = self.ptr_local(object);
                let target = Place {
                    base: mir::Base::Deref(p),
                    proj: Vec::new(),
                };
                if name == "read" {
                    return self.rvalue_to_temp(Rvalue::Use(Operand::Copy(target)), ty, e.span);
                }
                let v = self.expr(&args[0]);
                self.assign(target, Rvalue::Use(v), e.span);
                Operand::Const(Const::Void)
            }
            ExprKind::Member { object, name, .. }
                if matches!(self.ty_of(object), Ty::Array(_) | Ty::Range(_))
                    || (matches!(self.ty_of(object), Ty::Str) && name == "slice") =>
            {
                self.builtin_method(e, object, name, args, ty)
            }
            ExprKind::Member { object, name, .. } if name == "toString" => {
                let v = self.expr(object);
                let t = self.cx.types.final_ty(object.id).clone();
                self.stringify(v, &t, e.span)
            }
            ExprKind::Fn(f) if REFLECT_FNS.contains(&self.cx.program.func(*f).name.as_str()) => {
                // Reflection (spec §11.3): metadata and accessors made for these types.
                let name = self.cx.program.func(*f).name.clone();
                let args: Vec<Operand> = self.call_operands(e, *f, args);
                let targs: Vec<Ty> = self
                    .cx
                    .types
                    .instances
                    .get(&e.id)
                    .cloned()
                    .unwrap_or_default()
                    .iter()
                    .map(|t| self.mono(t))
                    .collect();
                if name.starts_with("__json")
                    && let Err(bad) =
                        ppl_sema::typeck::json_ok(self.cx.program, &targs[0], &mut Vec::new())
                {
                    let what = if bad == targs[0] {
                        format!("`{bad}`")
                    } else {
                        format!("`{}` (it holds a `{bad}`)", targs[0])
                    };
                    let _ = what;
                    self.cx
                        .diags
                        .push(ppl_sema::typeck::json_error(&targs[0], &bad, e.span));
                    return Operand::Const(Const::Undef(ty));
                }
                let what = match name.as_str() {
                    "__jsonRead" => Synth::JsonRead(targs[0].clone()),
                    "__jsonWrite" => Synth::JsonWrite(targs[0].clone()),
                    "__reflectType" | "__reflectTypeOf" => Synth::Info {
                        seen: targs[0].clone(),
                        of: targs[0].clone(),
                        file: e.span.file,
                        dynamic: name == "__reflectTypeOf",
                    },
                    "__reflectGet" => Synth::Get(targs[0].clone(), targs[1].clone()),
                    "__reflectSet" => Synth::Set(targs[0].clone(), targs[1].clone()),
                    _ => Synth::FieldShow(targs[0].clone()),
                };
                // `__reflectType<T>()` takes no value.
                let args = if name == "__reflectType" {
                    Vec::new()
                } else {
                    args
                };
                let id = self.cx.synth(what);
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
                match dest {
                    Some(p) => Operand::Copy(p),
                    None => Operand::Const(Const::Void),
                }
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
                let n = match self.ty_of(callee) {
                    Ty::Fn(ps, ..) => ps.len(),
                    _ => args.len(),
                };
                let mut ops = vec![f];
                ops.extend(args.iter().map(|a| self.expr(a)));
                // Left-out trailing `void` arguments.
                ops.extend((args.len()..n).map(|_| Operand::Const(Const::Void)));
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
                    let has_class = |t: &Ty| matches!(t.erase(), Ty::Class(..));
                    let with_class = matches!(&t, Ty::Union(_, ms) if ms.iter().any(has_class));
                    ops.push(
                        if with_class || matches!(t, Ty::Nullable(_) | Ty::Class(..)) {
                            self.stringify(v, &t, a.span)
                        } else {
                            v
                        },
                    );
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
            ExprKind::Builtin(Builtin::CatchPanic) => {
                // Guard { call f; Unguard → null } with a handler that stops the panic and
                // takes its message instead of resuming.
                let f = self.expr(&args[0]);
                let res_ty = Ty::Str.nullable();
                let res = Place::local(self.temp(res_ty.clone()));
                let (body, handler, done) = (self.new_block(), self.new_block(), self.new_block());
                self.terminate(Terminator::Guard { body, handler });
                self.switch_to(body);
                let next = self.new_block();
                self.terminate(Terminator::Call {
                    callee: Callee::Indirect,
                    args: vec![f],
                    dest: None,
                    next,
                    loc,
                });
                self.switch_to(next);
                self.unguard();
                self.assign(
                    res.clone(),
                    Rvalue::Use(Operand::Const(Const::Null(res_ty.clone()))),
                    e.span,
                );
                self.terminate(Terminator::Goto(done));
                self.switch_to(handler);
                let msg = Place::local(self.temp(Ty::Str));
                let after = self.new_block();
                self.terminate(Terminator::Call {
                    callee: Callee::Builtin(Builtin::PanicTaken),
                    args: Vec::new(),
                    dest: Some(msg.clone()),
                    next: after,
                    loc,
                });
                self.switch_to(after);
                self.assign(
                    res.clone(),
                    Rvalue::WrapSome(Operand::Copy(msg), res_ty),
                    e.span,
                );
                self.terminate(Terminator::Goto(done));
                self.switch_to(done);
                Operand::Copy(res)
            }
            ExprKind::Builtin(
                b @ (Builtin::GcCollect
                | Builtin::GcCollectYoung
                | Builtin::GcPause
                | Builtin::GcResume
                | Builtin::GcDumpRoots
                | Builtin::GcStat
                | Builtin::RefId
                | Builtin::RawArenaNew
                | Builtin::RawArenaFree
                | Builtin::ClockMs
                | Builtin::ParseF64
                | Builtin::Math
                | Builtin::LoopWait
                | Builtin::WorkerOp
                | Builtin::WorkerPost
                | Builtin::WorkerError
                | Builtin::BufOp
                | Builtin::Io
                | Builtin::IoText
                | Builtin::ComptimeInfo
                | Builtin::ComptimeReadFile
                | Builtin::MetaEmit),
            ) => {
                let args: Vec<Operand> = args.iter().map(|a| self.expr(a)).collect();
                let dest = matches!(
                    b,
                    Builtin::GcStat
                        | Builtin::RefId
                        | Builtin::RawArenaNew
                        | Builtin::ClockMs
                        | Builtin::ParseF64
                        | Builtin::Math
                        | Builtin::WorkerOp
                        | Builtin::WorkerError
                        | Builtin::BufOp
                        | Builtin::Io
                        | Builtin::IoText
                        | Builtin::ComptimeInfo
                        | Builtin::ComptimeReadFile
                )
                .then(|| {
                    let t = if matches!(
                        b,
                        Builtin::WorkerError
                            | Builtin::ComptimeInfo
                            | Builtin::ComptimeReadFile
                            | Builtin::IoText
                    ) {
                        Ty::Str
                    } else if matches!(b, Builtin::ParseF64 | Builtin::Math) {
                        Ty::Float(ppl_sema::ty::FloatTy::F64)
                    } else {
                        Ty::INT
                    };
                    Place::local(self.temp(t))
                });
                let next = self.new_block();
                self.terminate(Terminator::Call {
                    callee: Callee::Builtin(*b),
                    args,
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
