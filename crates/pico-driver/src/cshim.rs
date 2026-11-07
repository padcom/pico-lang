//! C shims for externs that pass `@pod` structs by value (spec §12.1, decision 330). C's
//! calling convention for structs differs by target (registers by size and field kinds on
//! SysV x86-64, homogeneous float aggregates on AArch64, by reference over 8 bytes on Win64,
//! by pointer on wasm32), so pico doesn't implement it: for each such extern `f` it writes
//!
//! ```c
//! struct pico_s3 f(struct pico_s1 a0, int32_t a1);
//! void __pico_shim_f(struct pico_s3 *out, struct pico_s1 *a0, int32_t a1) { *out = f(*a0, a1); }
//! ```
//!
//! with the structs declared as pico lays them out (fields in order, natural alignment), and
//! clang compiles that to LLVM bitcode for the target, which codegen links into the program's
//! module. pico's side of the call passes pointers to copies (`extern_body`).

use crate::target::Target;
use pico_mir::mir::Program;
use pico_sema::ty::{FloatTy, IntTy, StructId, Ty};
use std::fmt::Write;
use std::path::Path;

/// The shims' C source, or `None` if no extern passes a struct by value.
pub fn source(p: &Program) -> Option<String> {
    let mut structs: Vec<StructId> = Vec::new();
    let mut fns = String::new();
    let live = pico_mir::mir::live_functions(p);
    for (id, symbol) in &p.externs {
        if !live[id.0 as usize] {
            continue;
        }
        let f = &p.fns[id.0 as usize];
        let params: Vec<Ty> = f.params.iter().map(|l| f.local_ty(*l).clone()).collect();
        if !params
            .iter()
            .chain([&f.ret])
            .any(|t| matches!(t, Ty::Struct(..)))
        {
            continue;
        }
        for t in params.iter().chain([&f.ret]) {
            collect(p, t, &mut structs);
        }
        let ret_c = c_type(&f.ret, false);
        let decl: Vec<String> = params
            .iter()
            .enumerate()
            .map(|(i, t)| format!("{} a{i}", c_type(t, false)))
            .collect();
        let _ = writeln!(
            fns,
            "{ret_c} {symbol}({});",
            if decl.is_empty() {
                "void".to_string()
            } else {
                decl.join(", ")
            }
        );
        let mut shim_params = Vec::new();
        let by_out = matches!(f.ret, Ty::Struct(..));
        if by_out {
            shim_params.push(format!("{ret_c} *out"));
        }
        let mut args = Vec::new();
        for (i, t) in params.iter().enumerate() {
            if matches!(t, Ty::Struct(..)) {
                shim_params.push(format!("{} *a{i}", c_type(t, false)));
                args.push(format!("*a{i}"));
            } else {
                shim_params.push(format!("{} a{i}", c_type(t, false)));
                args.push(format!("a{i}"));
            }
        }
        let call = format!("{symbol}({})", args.join(", "));
        let body = if by_out {
            format!("*out = {call};")
        } else if f.ret == Ty::Void {
            format!("{call};")
        } else {
            format!("return {call};")
        };
        let shim_ret = if by_out { "void".to_string() } else { ret_c };
        let _ = writeln!(
            fns,
            "{shim_ret} __pico_shim_{symbol}({}) {{ {body} }}",
            if shim_params.is_empty() {
                "void".to_string()
            } else {
                shim_params.join(", ")
            }
        );
    }
    if fns.is_empty() {
        return None;
    }
    // (No headers: the compiler's own type macros work for every target.)
    let mut out = String::from(
        "typedef __INT8_TYPE__ int8_t;\ntypedef __INT16_TYPE__ int16_t;\n\
         typedef __INT32_TYPE__ int32_t;\ntypedef __INT64_TYPE__ int64_t;\n\
         typedef __UINT8_TYPE__ uint8_t;\ntypedef __UINT16_TYPE__ uint16_t;\n\
         typedef __UINT32_TYPE__ uint32_t;\ntypedef __UINT64_TYPE__ uint64_t;\n\
         typedef __SIZE_TYPE__ size_t;\ntypedef __PTRDIFF_TYPE__ ptrdiff_t;\n",
    );
    for sid in &structs {
        let l = p.layout(*sid);
        let _ = writeln!(out, "struct pico_s{} {{ /* {} */", sid.0, l.name);
        for (i, (_, t)) in l.fields.iter().enumerate() {
            let _ = writeln!(out, "  {};", field(t, &format!("f{i}")));
        }
        let _ = writeln!(out, "}};");
    }
    out.push_str(&fns);
    Some(out)
}

/// The structs `t` needs declared, dependencies first.
fn collect(p: &Program, t: &Ty, out: &mut Vec<StructId>) {
    match t {
        Ty::Struct(sid, ..) if !out.contains(sid) => {
            for (_, f) in &p.layout(*sid).fields {
                collect(p, f, out);
            }
            out.push(*sid);
        }
        Ty::Fixed(e, _) | Ty::Distinct(_, _, e) => collect(p, e, out),
        _ => {}
    }
}

/// A struct field `name` of type `t` (a fixed array is `T name[N]`).
fn field(t: &Ty, name: &str) -> String {
    match t {
        Ty::Fixed(e, n) => field(e, &format!("{name}[{n}]")),
        Ty::Distinct(_, _, b) => field(b, name),
        _ => format!("{} {name}", c_type(t, true)),
    }
}

/// `t` in C. In a struct field `isize`/`usize` keep pico's 64 bits; as a parameter or result
/// they're `ptrdiff_t`/`size_t`, as for other externs.
fn c_type(t: &Ty, in_struct: bool) -> String {
    match t {
        Ty::Void => "void".into(),
        Ty::Bool => "_Bool".into(),
        Ty::Char => "uint32_t".into(),
        Ty::Int(it) => match it {
            IntTy::I8 => "int8_t",
            IntTy::I16 => "int16_t",
            IntTy::I32 => "int32_t",
            IntTy::I64 => "int64_t",
            IntTy::U8 => "uint8_t",
            IntTy::U16 => "uint16_t",
            IntTy::U32 => "uint32_t",
            IntTy::U64 => "uint64_t",
            IntTy::Isize if in_struct => "int64_t",
            IntTy::Usize if in_struct => "uint64_t",
            IntTy::Isize => "ptrdiff_t",
            IntTy::Usize => "size_t",
        }
        .into(),
        Ty::Float(FloatTy::F32) => "float".into(),
        Ty::Float(_) => "double".into(),
        Ty::Str => "const char *".into(),
        Ty::Struct(sid, ..) => format!("struct pico_s{}", sid.0),
        Ty::Distinct(_, _, b) => c_type(b, in_struct),
        // Pointers (`ptr<T>`, `ptr<T>?`, callbacks).
        _ => "void *".into(),
    }
}

/// Compiles the shims to LLVM bitcode at `out` for `target`.
pub fn compile(src: &str, target: Target, out: &Path) -> Result<(), String> {
    let c = out.with_extension("c");
    std::fs::write(&c, src).map_err(|e| format!("cannot write {}: {e}", c.display()))?;
    let mut cmd = crate::link::clang()?.command();
    if !target.is_host() {
        cmd.arg(format!("--target={}", target.clang_triple()));
    }
    // (No tail calls: a struct passed `byval` is a copy in the shim's frame, which a tail call
    // would pop before the callee reads it; wasm, with its tail-call feature, did that.)
    cmd.args([
        "-O1",
        "-fno-optimize-sibling-calls",
        "-c",
        "-emit-llvm",
        "-w",
        "-o",
    ])
    .arg(out)
    .arg(&c);
    let r = cmd
        .output()
        .map_err(|e| format!("cannot run clang for the C shims: {e}"))?;
    let _ = std::fs::remove_file(&c);
    if !r.status.success() {
        return Err(format!(
            "compiling the C shims failed:\n{}\n{src}",
            String::from_utf8_lossy(&r.stderr)
        ));
    }
    Ok(())
}
