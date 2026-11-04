//! Compile-time evaluation (spec §11.1): runs every `comptime` expression of a program in the
//! interpreter and replaces it with its value, written out as MIR constants.
//!
//! The sites run in order (the entry's code first, which is source order), all in one
//! interpreter: a `comptime const` global gets its value there, so later comptime code can
//! use it. Other globals are only set when the program runs; comptime code reading one fails.

use crate::{Interp, Loc, Panic, Value, cell, workers};
use pico_diag::{Diagnostic, Span, codes};
use pico_mir::mir::{
    Base, Const, Function, Local, LocalDecl, Operand, Place, Program, Rvalue, Statement,
};
use pico_sema::hir::{Builtin, FnId, GlobalId};
use pico_sema::{FloatTy, Ty};
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

/// Evaluation steps (statements and terminators) one top-level comptime evaluation may take,
/// unless its declaration says otherwise with `@comptimeLimit(n)`.
pub const DEFAULT_LIMIT: u64 = 10_000_000;

/// The panic message of an evaluation that ran out of steps.
pub(crate) const LIMIT_PANIC: &str = "comptime evaluation exceeded its step limit";

/// What comptime code can know about the build.
#[derive(Clone, Debug)]
pub struct Config {
    /// The package root: `comptime.readFile` paths are relative to it and stay inside it.
    pub root: PathBuf,
    /// `target.os`: `"linux"`, `"macos"`, `"windows"`, `"wasi"` or `"browser"`.
    pub os: String,
    /// `target.arch`: `"aarch64"`, `"x86_64"`, `"wasm32"`.
    pub arch: String,
    /// `target.triple`, e.g. `"aarch64-apple-darwin"`.
    pub triple: String,
    /// `build.mode`: `"debug"` or `"release"`.
    pub mode: String,
}

impl Config {
    /// Building for the machine the compiler runs on.
    pub fn host(root: PathBuf, release: bool) -> Config {
        let arch = std::env::consts::ARCH.to_string();
        let os = std::env::consts::OS.to_string();
        let triple = match os.as_str() {
            "macos" => format!("{arch}-apple-darwin"),
            "windows" => format!("{arch}-w64-windows-gnu"),
            "linux" => format!("{arch}-unknown-linux-gnu"),
            other => format!("{arch}-unknown-{other}"),
        };
        Config {
            root,
            os,
            arch,
            triple,
            mode: if release { "release" } else { "debug" }.into(),
        }
    }
}

/// The interpreter's comptime mode.
pub(crate) struct State {
    pub cfg: Config,
    pub steps: u64,
    pub limit: u64,
    /// What decorators asked for (`__meta_emit`).
    pub meta: Vec<MetaEdit>,
}

/// One request of a decorator (`std/meta`): the decorated declaration's id, what to do
/// (`"error"`, `"member"`, `"wrap"`, …) and its two string arguments.
#[derive(Clone, Debug, PartialEq)]
pub struct MetaEdit {
    pub decl: i64,
    pub kind: String,
    pub a: String,
    pub b: String,
}

impl Interp<'_, '_> {
    /// Comptime-only builtins, and the runtime ones comptime code can't use; `None` for the
    /// rest.
    pub(crate) fn comptime_builtin(
        &mut self,
        b: Builtin,
        vals: &[Value],
        loc: Loc,
    ) -> Option<Result<Value, Panic>> {
        let Some(st) = &self.comptime else {
            return matches!(
                b,
                Builtin::ComptimeInfo | Builtin::ComptimeReadFile | Builtin::MetaEmit
            )
            .then(|| {
                Err(self.panic(
                    &format!("`{}` is only available at comptime", b.name()),
                    loc,
                ))
            });
        };
        Some(match b {
            Builtin::ComptimeInfo => {
                let k = match vals[0] {
                    Value::Int(k) => k,
                    _ => -1,
                };
                let s = match k {
                    0 => &st.cfg.os,
                    1 => &st.cfg.arch,
                    2 => &st.cfg.triple,
                    3 => &st.cfg.mode,
                    _ => return Some(Err(self.panic("unknown comptime fact", loc))),
                };
                Ok(Value::Str(s.as_str().into()))
            }
            Builtin::MetaEmit => {
                let s = |v: &Value| match v {
                    Value::Str(s) => s.to_string(),
                    _ => String::new(),
                };
                let decl = match vals[0] {
                    Value::Int(k) => k as i64,
                    _ => -1,
                };
                let edit = MetaEdit {
                    decl,
                    kind: s(&vals[1]),
                    a: s(&vals[2]),
                    b: s(&vals[3]),
                };
                if let Some(st) = &mut self.comptime {
                    st.meta.push(edit);
                }
                Ok(Value::Void)
            }
            Builtin::ComptimeReadFile => {
                let Value::Str(path) = &vals[0] else {
                    unreachable!("a string path")
                };
                read_file(&st.cfg.root, path)
                    .map(|s| Value::Str(s.into()))
                    .map_err(|m| self.panic(&m, loc))
            }
            Builtin::ClockMs
            | Builtin::LoopWait
            | Builtin::WorkerOp
            | Builtin::WorkerPost
            | Builtin::WorkerError => {
                Err(self.panic(&format!("`{}` isn't available at comptime", b.name()), loc))
            }
            _ => return None,
        })
    }
}

thread_local! {
    /// The files `comptime.readFile` read on this thread (inputs of the build, spec §14.6).
    static FILES_READ: std::cell::RefCell<Vec<std::path::PathBuf>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// The files `comptime.readFile` read on this thread since the last call (and forgets them).
pub fn take_files_read() -> Vec<std::path::PathBuf> {
    FILES_READ.with(|f| std::mem::take(&mut *f.borrow_mut()))
}

/// `comptime.readFile(path)`: only relative paths that stay inside the package root.
fn read_file(root: &Path, path: &str) -> Result<String, String> {
    let rel = Path::new(path);
    let inside = rel
        .components()
        .all(|c| matches!(c, Component::Normal(_) | Component::CurDir));
    if !inside || path.is_empty() {
        return Err(format!(
            "comptime.readFile(\"{path}\"): the path must be relative and stay inside the package root"
        ));
    }
    let full = root.join(rel);
    FILES_READ.with(|f| f.borrow_mut().push(full.clone()));
    std::fs::read_to_string(full).map_err(|e| format!("comptime.readFile(\"{path}\"): {e}"))
}

/// One `comptime` expression in the MIR.
struct Site {
    thunk: FnId,
    limit: Option<u64>,
    /// The `comptime const` global the value is stored into next.
    global: Option<GlobalId>,
    span: Option<Span>,
}

/// Evaluates every comptime expression of `p` and puts the values in their place. Returns
/// the diagnostics and what comptime code printed.
pub fn run(p: &mut Program, cfg: &Config) -> (Vec<Diagnostic>, String) {
    let (d, out, _) = run_meta(p, cfg);
    (d, out)
}

/// [`run`], also returning what decorators asked for (in order).
pub fn run_meta(p: &mut Program, cfg: &Config) -> (Vec<Diagnostic>, String, Vec<MetaEdit>) {
    let sites = sites(p);
    let mut meta = Vec::new();
    if sites.is_empty() {
        return (Vec::new(), String::new(), meta);
    }
    let mut diags = Vec::new();
    let mut values: HashMap<FnId, Value> = HashMap::new();
    let output;
    {
        let shared = Arc::new(workers::Shared::default());
        let mut it = Interp::new(p, None, shared.clone(), None);
        for site in &sites {
            if values.contains_key(&site.thunk) {
                continue;
            }
            let limit = site.limit.unwrap_or(DEFAULT_LIMIT);
            if let Some(st) = it.comptime.take() {
                meta.extend(st.meta);
            }
            it.comptime = Some(State {
                cfg: cfg.clone(),
                steps: 0,
                limit,
                meta: Vec::new(),
            });
            let env = Value::Ref(cell(Value::Struct(Vec::new())), Rc::from(Vec::new()));
            let r = it.call(site.thunk, vec![env], Loc::default());
            it.depth = 0;
            it.stack.clear();
            it.frames.clear();
            it.unwinding.clear();
            match r {
                Ok(v) if it.exception.is_none() => {
                    if let Some(g) = site.global {
                        *it.globals[g.0 as usize].borrow_mut() = v.clone();
                    }
                    values.insert(site.thunk, v);
                }
                Ok(_) => {
                    let e = it.exception.take().expect("pending");
                    let what = match &e {
                        Value::Object(o) => {
                            let class = &p.structs[o.class as usize].name;
                            match o.fields.borrow().first() {
                                Some(Value::Str(m)) => format!("{class}: {m}"),
                                _ => class.clone(),
                            }
                        }
                        _ => "a lightweight error".into(),
                    };
                    if let Some(span) = site.span {
                        diags.push(
                            Diagnostic::error(
                                codes::COMPTIME_FAILED,
                                format!("comptime code threw `{what}`"),
                            )
                            .primary(span, "evaluated at compile time"),
                        );
                    }
                }
                Err(panic) => {
                    if let Some(span) = site.span {
                        diags.push(failure(&panic, span, limit));
                    }
                }
            }
        }
        if let Some(st) = it.comptime.take() {
            meta.extend(st.meta);
        }
        output = String::from_utf8_lossy(&std::mem::take(&mut *shared.out.lock().expect("output")))
            .into_owned();
    }
    // Write the values in.
    for f in &mut p.fns {
        let mut b = 0;
        while b < f.blocks.len() {
            let mut i = 0;
            while i < f.blocks[b].stmts.len() {
                let st = &f.blocks[b].stmts[i];
                let Rvalue::Comptime(thunk, _) = st.rvalue else {
                    i += 1;
                    continue;
                };
                let Some(v) = values.get(&thunk) else {
                    i += 1;
                    continue;
                };
                let (place, loc) = (st.place.clone(), st.loc);
                let layouts = Layouts {
                    structs: &p.structs,
                    enums: &p.enums,
                };
                let ty = place_ty(layouts, f, &place, &p.globals);
                let mut out = Vec::new();
                match write_value(layouts, f, &mut out, v, &ty, loc) {
                    Ok(o) => {
                        out.push(Statement {
                            place,
                            rvalue: Rvalue::Use(o),
                            loc,
                        });
                    }
                    Err(what) => {
                        if let Some(span) = loc.span {
                            diags.push(
                                Diagnostic::error(
                                    codes::COMPTIME_VALUE,
                                    format!("a comptime value can't contain {what}"),
                                )
                                .primary(span, "evaluated at compile time")
                                .note("comptime values are numbers, strings, structs, tuples, arrays, enums and nullables of those"),
                            );
                        }
                        i += 1;
                        continue;
                    }
                }
                let n = out.len();
                f.blocks[b].stmts.splice(i..=i, out);
                i += n;
            }
            b += 1;
        }
    }
    // The thunks are left only for comptime code; they compile only if something else
    // still reaches them.
    for s in &sites {
        p.fns[s.thunk.0 as usize].library = true;
    }
    (diags, output, meta)
}

fn failure(panic: &Panic, span: Span, limit: u64) -> Diagnostic {
    let mut d = if panic.msg == LIMIT_PANIC {
        Diagnostic::error(
            codes::COMPTIME_LIMIT,
            format!("comptime evaluation took more than {limit} steps"),
        )
        .primary(span, "evaluated at compile time")
        .help("if it isn't stuck in a loop, raise the limit with `@comptimeLimit(n)` on the declaration")
    } else {
        Diagnostic::error(
            codes::COMPTIME_FAILED,
            format!("comptime evaluation panicked: {}", panic.msg),
        )
        .primary(span, "evaluated at compile time")
    };
    if let Some(at) = panic.loc.span
        && at != span
    {
        d = d.secondary(at, "here");
    }
    if !panic.trace.is_empty() {
        let frames: Vec<String> = panic
            .trace
            .iter()
            .enumerate()
            .map(|(i, f)| format!("  {i}: {f}"))
            .collect();
        d = d.note(format!("comptime backtrace:\n{}", frames.join("\n")));
    }
    d
}

/// The comptime expressions, in evaluation order: the entry's (in block order), then the
/// other functions'.
fn sites(p: &Program) -> Vec<Site> {
    let entry = p.entry.0 as usize;
    let order = std::iter::once(entry).chain((0..p.fns.len()).filter(|i| *i != entry));
    let mut out = Vec::new();
    for fi in order {
        let f = &p.fns[fi];
        for b in &f.blocks {
            for (i, st) in b.stmts.iter().enumerate() {
                let Rvalue::Comptime(thunk, limit) = st.rvalue else {
                    continue;
                };
                // `_t = comptime …; @g = _t`: a `comptime const` global.
                let global = match (fi == entry, &st.place.base, b.stmts.get(i + 1)) {
                    (true, Base::Local(t), Some(next)) if st.place.proj.is_empty() => {
                        match (&next.place, &next.rvalue) {
                            (
                                Place {
                                    base: Base::Global(g),
                                    proj,
                                },
                                Rvalue::Use(Operand::Copy(src)),
                            ) if proj.is_empty() && *src == Place::local(*t) => Some(*g),
                            _ => None,
                        }
                    }
                    _ => None,
                };
                out.push(Site {
                    thunk,
                    limit,
                    global,
                    span: st.loc.span,
                });
            }
        }
    }
    out
}

/// The program's struct and enum layouts (what writing values out needs).
#[derive(Clone, Copy)]
struct Layouts<'a> {
    structs: &'a [pico_mir::mir::StructLayout],
    enums: &'a [pico_mir::mir::EnumLayout],
}

/// [`Program::place_ty`] while the function is borrowed out of the program.
fn place_ty(l: Layouts, f: &Function, place: &Place, globals: &[pico_mir::mir::GlobalDecl]) -> Ty {
    let mut t = match place.base {
        Base::Local(l) => f.local_ty(l).clone(),
        Base::Global(g) => globals[g.0 as usize].ty.clone(),
        Base::Deref(l) => match f.local_ty(l) {
            Ty::Ref(inner) => (**inner).clone(),
            other => unreachable!("deref of {other}"),
        },
    };
    let field = |r: pico_mir::mir::FieldRef| match r {
        pico_mir::mir::FieldRef::Struct(sid, i) => l.structs[sid.0 as usize].fields[i].1.clone(),
        pico_mir::mir::FieldRef::Variant(eid, v, i) => l.enums[eid.0 as usize].variants[v as usize]
            .fields[i]
            .1
            .clone(),
    };
    for pr in &place.proj {
        t = pico_mir::mir::project(&t, pr, field);
    }
    t
}

/// An operand holding `v` (a `ty`), with the statements that build it appended to `out`;
/// `Err` names what can't be written out.
fn write_value(
    l: Layouts,
    f: &mut Function,
    out: &mut Vec<Statement>,
    v: &Value,
    ty: &Ty,
    loc: Loc,
) -> Result<Operand, String> {
    let ty = ty.erase();
    let temp = |f: &mut Function, out: &mut Vec<Statement>, rv: Rvalue| {
        let local = Local(f.locals.len() as u32);
        f.locals.push(LocalDecl {
            ty: ty.clone(),
            name: None,
        });
        out.push(Statement {
            place: Place::local(local),
            rvalue: rv,
            loc,
        });
        Operand::Copy(Place::local(local))
    };
    Ok(match (v, &ty) {
        (Value::Int(x), Ty::Int(it)) => Operand::Const(Const::Int(*x, *it)),
        (Value::Float(x), Ty::Float(ft)) => Operand::Const(Const::Float(
            match ft {
                FloatTy::F32 => *x as f32 as f64,
                FloatTy::F64 => *x,
            },
            *ft,
        )),
        (Value::Bool(b), Ty::Bool) => Operand::Const(Const::Bool(*b)),
        (Value::Char(c), Ty::Char) => Operand::Const(Const::Char(*c)),
        (Value::Str(s), Ty::Str) => Operand::Const(Const::Str(s.to_string())),
        (Value::Void, Ty::Void) => Operand::Const(Const::Void),
        (Value::Null, Ty::Nullable(_)) => Operand::Const(Const::Null(ty.clone())),
        (v, Ty::Nullable(inner)) => {
            let o = write_value(l, f, out, v, inner, loc)?;
            temp(f, out, Rvalue::WrapSome(o, ty.clone()))
        }
        (Value::Struct(parts), Ty::Struct(sid, _, args)) => {
            let owner = Ty::type_owner(*sid);
            let fields: Vec<Ty> = l.structs[sid.0 as usize]
                .fields
                .iter()
                .map(|(_, t)| t.subst(owner, args))
                .collect();
            let mut ops = Vec::new();
            for (part, ft) in parts.iter().zip(&fields) {
                ops.push(write_value(l, f, out, part, ft, loc)?);
            }
            temp(f, out, Rvalue::Aggregate(ty.clone(), ops))
        }
        (Value::Struct(parts), Ty::Tuple(_) | Ty::Fixed(..) | Ty::Range(_)) => {
            let part_ty = |i: usize| match &ty {
                Ty::Tuple(ts) => ts[i].clone(),
                Ty::Fixed(e, _) => (**e).clone(),
                Ty::Range(it) => Ty::Int(*it),
                _ => unreachable!(),
            };
            let mut ops = Vec::new();
            for (i, part) in parts.iter().enumerate() {
                ops.push(write_value(l, f, out, part, &part_ty(i), loc)?);
            }
            temp(f, out, Rvalue::Tuple(ty.clone(), ops))
        }
        (Value::Enum(var, payload), Ty::Enum(eid, _)) => {
            let fields: Vec<Ty> = l.enums[eid.0 as usize].variants[*var as usize]
                .fields
                .iter()
                .map(|(_, t)| t.clone())
                .collect();
            let mut ops = Vec::new();
            for (part, ft) in payload.iter().zip(&fields) {
                ops.push(write_value(l, f, out, part, ft, loc)?);
            }
            temp(f, out, Rvalue::Variant(*eid, *var, ops))
        }
        (Value::Array(a), Ty::Array(e)) => {
            let mut ops = Vec::new();
            for x in a.borrow().iter() {
                ops.push(write_value(l, f, out, x, e, loc)?);
            }
            temp(f, out, Rvalue::ArrayNew((**e).clone(), ops))
        }
        (Value::Object(_), _) => return Err(format!("an object (`{ty}`)")),
        (Value::Closure(_), _) => return Err(format!("a function (`{ty}`)")),
        (_, _) => return Err(format!("a `{ty}`")),
    })
}
