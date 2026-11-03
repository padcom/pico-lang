//! wasm32-browser (spec §12.3, decision 258): the program is a wasm module on linear memory
//! with the ppl GC, plus ES module glue (`app.js`) that instantiates it, stands in for the
//! handful of WASI preview 1 calls wasi-libc makes (output to the console, clocks, random,
//! arguments and environment) and drives the event loop from browser timers.
//!
//! `@export("js")` functions get JavaScript functions in the glue and declarations in
//! `app.d.ts`: numbers, `bool` and `string` convert, structs are copied to and from plain
//! objects (laid out in linear memory as the generated code lays them out), and class
//! instances are handles that keep their object alive until they're freed or collected.

use ppl_mir::mir::{Export, Program};
use ppl_sema::hir::ExportAbi;
use ppl_sema::ty::{FloatTy, IntTy, Ty};
use std::fmt::Write;
use std::path::{Path, PathBuf};

const GLUE: &str = include_str!("browser/glue.js");
/// Pointers are 4 bytes on wasm32.
const PTR: u64 = 4;

/// The files a browser build writes for `out` (`app`, `app.js` or `app.wasm`).
pub struct Files {
    pub wasm: PathBuf,
    pub js: PathBuf,
    pub dts: PathBuf,
}

pub fn files(out: &Path) -> Files {
    let base = match out.extension().and_then(|e| e.to_str()) {
        Some("js" | "wasm") => out.with_extension(""),
        _ => out.to_path_buf(),
    };
    let name = base.file_name().unwrap_or_default().to_string_lossy();
    Files {
        wasm: base.with_file_name(format!("{name}.wasm")),
        js: base.with_file_name(format!("{name}.js")),
        dts: base.with_file_name(format!("{name}.d.ts")),
    }
}

/// Links `obj` into the module and writes the glue and its declarations next to it.
pub fn build(mir: &Program, obj: &Path, out: &Path, packages: &[String]) -> Result<(), String> {
    let f = files(out);
    let t = crate::target::Target::parse("wasm32-browser").expect("a known target");
    crate::link::link_browser(t, obj, &f.wasm)?;
    let wasm = f.wasm.file_name().unwrap_or_default().to_string_lossy();
    let base = wasm.trim_end_matches(".wasm");
    let mut g = Gen {
        p: mir,
        js: String::new(),
        dts: String::new(),
        classes: Vec::new(),
        structs: Vec::new(),
    };
    let exports: Vec<&Export> = mir
        .exports
        .iter()
        // (`__` exports are the library's own: std/js's callback entry.)
        .filter(|e| e.abi == ExportAbi::Js && !e.name.starts_with("__"))
        .collect();
    for e in &exports {
        g.export(e);
    }
    let glue = GLUE
        .replace("__PPL_WASM__", &wasm)
        .replace("__PPL_BASE__", base)
        .replace("/*__PPL_EXPORTS__*/\n", &g.js)
        .replace("/*__PPL_JS_MODULES__*/\n", &js_modules(packages));
    write(&f.js, &glue)?;
    write(&f.dts, &g.declarations(base))
}

/// The glue's imports of the npm packages `js:npm/…` names (resolved by the page's import map
/// or bundler in a browser, by `node_modules` under Node.js).
fn js_modules(packages: &[String]) -> String {
    let mut out = String::new();
    for p in packages {
        let _ = writeln!(
            out,
            "  jsModules[{:?}] = await import({p:?});",
            format!("npm/{p}")
        );
    }
    out
}

fn write(path: &Path, text: &str) -> Result<(), String> {
    std::fs::write(path, text).map_err(|e| format!("cannot write {}: {e}", path.display()))
}

struct Gen<'a> {
    p: &'a Program,
    /// The export functions (inside `init`).
    js: String,
    /// The `App` interface's members.
    dts: String,
    /// Handle classes (by name) and structs (by name) used so far.
    classes: Vec<String>,
    structs: Vec<ppl_sema::hir::StructId>,
}

impl Gen<'_> {
    fn export(&mut self, e: &Export) {
        let sym = crate::export_symbol(e);
        let params: Vec<String> = e.params.iter().map(|(n, _)| js_name(n)).collect();
        let mut body = String::new();
        let mut args = Vec::new();
        let out = matches!(e.ret, Ty::Struct(..));
        if out {
            let (size, _) = self.p.size_align_with(&e.ret, PTR);
            let _ = writeln!(body, "    const out = ex.ppl_js_alloc({size});");
            let _ = writeln!(body, "    temps.push(() => ex.ppl_js_free(out));");
            args.push("out".to_string());
        }
        for (name, (_, t)) in params.iter().zip(&e.params) {
            if let Ty::Struct(..) = t {
                let (size, _) = self.p.size_align_with(t, PTR);
                let buf = format!("{name}$");
                let _ = writeln!(body, "    const {buf} = ex.ppl_js_alloc({size});");
                let _ = writeln!(body, "    temps.push(() => ex.ppl_js_free({buf}));");
                let store = self.store(t, &buf, "0", name);
                let _ = writeln!(body, "    {store}");
                args.push(buf);
            } else {
                args.push(self.js_to_wasm(t, name));
            }
        }
        let callexpr = format!("ex.{sym}({})", args.join(", "));
        if out {
            let _ = writeln!(body, "    {callexpr};");
            let load = self.load(&e.ret, "out", "0");
            let _ = writeln!(body, "    return {load};");
        } else if matches!(e.ret, Ty::Void) {
            let _ = writeln!(body, "    {callexpr};");
        } else {
            let _ = writeln!(body, "    const r = {callexpr};");
            let _ = writeln!(body, "    return {};", self.wasm_to_js(&e.ret, "r"));
        }
        let _ = writeln!(
            self.js,
            "  app[{name:?}] = ({ps}) =>\n    call((temps) => {{\n{body}    }});",
            name = e.name,
            ps = params.join(", "),
        );
        let dps: Vec<String> = params
            .iter()
            .zip(&e.params)
            .map(|(n, (_, t))| format!("{n}: {}", self.ts(t)))
            .collect();
        let ret = self.ts(&e.ret);
        let _ = writeln!(self.dts, "  {}({}): {ret};", e.name, dps.join(", "));
    }

    /// A JavaScript value as the wasm function takes it (not structs).
    fn js_to_wasm(&mut self, t: &Ty, v: &str) -> String {
        match t {
            Ty::Int(it) if it.bits() == 64 => {
                if it.signed() {
                    format!("i64({v})")
                } else {
                    format!("u64({v})")
                }
            }
            Ty::Int(_) | Ty::Float(_) => format!("+{v}"),
            Ty::Bool => format!("({v} ? 1 : 0)"),
            Ty::Str => format!("str({v}, temps)"),
            Ty::Class(_, name, _) => {
                self.class(name);
                format!("ptrOf({v}, {})", js_name(name))
            }
            _ => unreachable!("checked by lowering"),
        }
    }

    /// A wasm result as JavaScript sees it (not structs).
    fn wasm_to_js(&mut self, t: &Ty, v: &str) -> String {
        match t {
            Ty::Int(it) if it.bits() == 64 => format!("Number({v})"),
            Ty::Int(IntTy::U32) => format!("{v} >>> 0"),
            Ty::Int(_) | Ty::Float(_) => v.to_string(),
            Ty::Bool => format!("{v} !== 0"),
            Ty::Str => format!("readStr({v})"),
            Ty::Class(_, name, _) => {
                self.class(name);
                format!("new {}({v})", js_name(name))
            }
            _ => unreachable!("checked by lowering"),
        }
    }

    /// Statements storing JavaScript value `val` of type `t` at `base + off`. (Every access
    /// takes a new view: allocating a string can grow the memory, detaching old views.)
    fn store(&mut self, t: &Ty, base: &str, off: &str, val: &str) -> String {
        let at = format!("{base} + {off}");
        match t {
            Ty::Int(it) => {
                let (m, conv) = match (it.bits(), it.signed()) {
                    (8, true) => ("setInt8", val.to_string()),
                    (8, false) => ("setUint8", val.to_string()),
                    (16, true) => ("setInt16", val.to_string()),
                    (16, false) => ("setUint16", val.to_string()),
                    (32, true) => ("setInt32", val.to_string()),
                    (32, false) => ("setUint32", val.to_string()),
                    (_, true) => ("setBigInt64", format!("i64({val})")),
                    (_, false) => ("setBigUint64", format!("u64({val})")),
                };
                if it.bits() == 8 {
                    format!("view().{m}({at}, {conv});")
                } else {
                    format!("view().{m}({at}, {conv}, true);")
                }
            }
            Ty::Float(FloatTy::F32) => format!("view().setFloat32({at}, {val}, true);"),
            Ty::Float(_) => format!("view().setFloat64({at}, {val}, true);"),
            Ty::Bool => format!("view().setUint8({at}, {val} ? 1 : 0);"),
            Ty::Str | Ty::Class(..) => {
                let p = self.js_to_wasm(t, val);
                format!("view().setUint32({at}, {p}, true);")
            }
            Ty::Struct(sid, name, _) => {
                self.structs_push(*sid);
                let mut s = format!(
                    "if (typeof {val} !== \"object\" || {val} === null) throw new ArgError(\"expected a {name} object\");"
                );
                for (fname, fty, foff) in self.fields(*sid) {
                    let inner = format!("{val}[{fname:?}]");
                    s.push(' ');
                    s.push_str(&self.store(&fty, base, &format!("{off} + {foff}"), &inner));
                }
                s
            }
            _ => unreachable!("checked by lowering"),
        }
    }

    /// An expression reading a value of type `t` at `base + off`.
    fn load(&mut self, t: &Ty, base: &str, off: &str) -> String {
        let at = format!("{base} + {off}");
        match t {
            Ty::Int(it) => match (it.bits(), it.signed()) {
                (8, true) => format!("view().getInt8({at})"),
                (8, false) => format!("view().getUint8({at})"),
                (16, true) => format!("view().getInt16({at}, true)"),
                (16, false) => format!("view().getUint16({at}, true)"),
                (32, true) => format!("view().getInt32({at}, true)"),
                (32, false) => format!("view().getUint32({at}, true)"),
                (_, true) => format!("Number(view().getBigInt64({at}, true))"),
                (_, false) => format!("Number(view().getBigUint64({at}, true))"),
            },
            Ty::Float(FloatTy::F32) => format!("view().getFloat32({at}, true)"),
            Ty::Float(_) => format!("view().getFloat64({at}, true)"),
            Ty::Bool => format!("view().getUint8({at}) !== 0"),
            Ty::Str | Ty::Class(..) => {
                let p = format!("view().getUint32({at}, true)");
                self.wasm_to_js(t, &p)
            }
            Ty::Struct(sid, _, _) => {
                self.structs_push(*sid);
                let parts: Vec<String> = self
                    .fields(*sid)
                    .into_iter()
                    .map(|(fname, fty, foff)| {
                        let l = self.load(&fty, base, &format!("{off} + {foff}"));
                        format!("{fname:?}: {l}")
                    })
                    .collect();
                format!("{{ {} }}", parts.join(", "))
            }
            _ => unreachable!("checked by lowering"),
        }
    }

    /// A struct's fields with their offsets (as the generated code lays them out on wasm32).
    fn fields(&self, sid: ppl_sema::hir::StructId) -> Vec<(String, Ty, u64)> {
        let mut off: u64 = 0;
        let mut out = Vec::new();
        for (name, t) in &self.p.layout(sid).fields {
            let (size, align) = self.p.size_align_with(t, PTR);
            if size == 0 {
                continue;
            }
            off = off.div_ceil(align) * align;
            out.push((name.clone(), t.clone(), off));
            off += size;
        }
        out
    }

    fn structs_push(&mut self, sid: ppl_sema::hir::StructId) {
        if !self.structs.contains(&sid) {
            self.structs.push(sid);
        }
    }

    fn class(&mut self, name: &str) {
        if !self.classes.iter().any(|c| c == name) {
            self.classes.push(name.to_string());
            let n = js_name(name);
            let _ = writeln!(
                self.js,
                "  class {n} extends Handle {{}}\n  app[{name:?}] = {n};"
            );
        }
    }

    /// The TypeScript type of `t`.
    fn ts(&mut self, t: &Ty) -> String {
        match t {
            Ty::Void => "void".into(),
            Ty::Int(_) | Ty::Float(_) => "number".into(),
            Ty::Bool => "boolean".into(),
            Ty::Str => "string".into(),
            Ty::Struct(sid, name, _) => {
                self.structs_push(*sid);
                name.to_string()
            }
            Ty::Class(_, name, _) => {
                self.class(name);
                name.to_string()
            }
            _ => "unknown".into(),
        }
    }

    /// `app.d.ts`.
    fn declarations(&mut self, base: &str) -> String {
        let mut out =
            format!("// Generated by ppl for {base}.wasm (wasm32-browser): the exported API.\n\n");
        // Struct interfaces (their fields may name more structs).
        let mut i = 0;
        let mut decls = String::new();
        while i < self.structs.len() {
            let sid = self.structs[i];
            let name = self.p.layout(sid).name.clone();
            let fields: Vec<String> = self
                .fields(sid)
                .into_iter()
                .map(|(f, t, _)| format!("  {f}: {};", self.ts(&t)))
                .collect();
            let _ = writeln!(
                decls,
                "export interface {name} {{\n{}\n}}\n",
                fields.join("\n")
            );
            i += 1;
        }
        out.push_str(&decls);
        for c in &self.classes {
            let _ = writeln!(
                out,
                "/** A handle to a ppl `{c}`: keeps it alive until freed (or collected). */\nexport declare class {c} {{\n  private constructor();\n  free(): void;\n  [Symbol.dispose](): void;\n}}\n"
            );
        }
        let _ = write!(
            out,
            "export interface App {{\n  readonly instance: WebAssembly.Instance;\n  /** The exit code, once nothing is pending (or the program exits). */\n  readonly done: Promise<number>;\n{}",
            self.dts
        );
        for c in &self.classes {
            let _ = writeln!(out, "  readonly {c}: typeof {c};");
        }
        out.push_str("}\n\n");
        out.push_str(
            "export interface InitOptions {\n  module?: string | URL | BufferSource | WebAssembly.Module;\n  name?: string;\n  args?: string[];\n  env?: Record<string, string>;\n  stdout?: (bytes: Uint8Array) => void;\n  stderr?: (bytes: Uint8Array) => void;\n}\n\n/** Instantiates the program and runs its top-level code. */\nexport default function init(options?: InitOptions): Promise<App>;\n",
        );
        out
    }
}

/// A JavaScript identifier for a ppl name (`$` for characters JavaScript doesn't allow).
fn js_name(n: &str) -> String {
    let s: String = n
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '_' || c == '$' {
                c
            } else {
                '$'
            }
        })
        .collect();
    match s.as_str() {
        "default" | "class" | "function" | "var" | "new" | "delete" | "in" | "with" | "this"
        | "out" | "temps" | "v" | "r" | "ex" | "app" | "call" | "str" => format!("{s}$"),
        _ => s,
    }
}
