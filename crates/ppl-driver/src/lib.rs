//! Compilation pipeline, caching, targets and linking for ppl.
//!
//! [`check_source`] runs the front end: parsing, name resolution, type checking, and lowering
//! to MIR. Later stages plug in
//! there as they're built, and [`Stage::IMPLEMENTED`] tells tools which diagnostics can
//! currently be produced. Each stage runs only if the previous ones reported no errors.

use ppl_diag::{Code, Diagnostic, SourceMap};
use ppl_syntax::ast::Module;
use std::path::{Path, PathBuf};

mod browser;
mod builtins;
mod cheader;
mod clib;
mod decorators;
mod dts;
mod glibc;
mod link;
mod mingw;
mod modules;
mod musl;
pub mod target;
pub use ppl_codegen::export_symbol;
/// What the WasmGC backend's "not supported yet" errors say (P8.7).
pub const WASMGC_UNSUPPORTED: &str = ppl_codegen::wasmgc::UNSUPPORTED;
/// Whether this compiler links with lld (needed for every target but a macOS host's own).
pub const LLD_AVAILABLE: bool = ppl_lld::AVAILABLE;
mod wasi;

pub use modules::stdlib_dir;
pub use target::Target;

/// Compiler stages, in pipeline order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Stage {
    /// Lexing and parsing (`E00xx`, `E01xx`).
    Parse,
    /// Name resolution and modules (`E02xx`).
    Resolve,
    /// Type checking (`E03xx` and above).
    Typecheck,
}

impl Stage {
    /// The stages the compiler runs today.
    pub const IMPLEMENTED: &'static [Stage] = &[Stage::Parse, Stage::Resolve, Stage::Typecheck];

    pub fn is_implemented(self) -> bool {
        Stage::IMPLEMENTED.contains(&self)
    }

    /// The stage that reports `code` (from the ranges in `ppl_diag::codes`).
    pub fn of(code: Code) -> Stage {
        match code.0 {
            0..=199 => Stage::Parse,
            200..=299 => Stage::Resolve,
            _ => Stage::Typecheck,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Stage::Parse => "parser",
            Stage::Resolve => "name resolution",
            Stage::Typecheck => "type checker",
        }
    }
}

/// The result of running the front end on one file.
pub struct Checked {
    pub sm: SourceMap,
    pub module: Module,
    /// The resolved program, if parsing succeeded.
    pub hir: Option<ppl_sema::hir::Program>,
    /// Type information, if resolution succeeded.
    pub types: Option<ppl_sema::Types>,
    /// The MIR, if type checking succeeded.
    pub mir: Option<ppl_mir::Program>,
    pub diags: Vec<Diagnostic>,
    /// What `comptime` code printed (the compile-time log).
    pub comptime_output: String,
}

impl Checked {
    pub fn has_errors(&self) -> bool {
        self.diags.iter().any(Diagnostic::is_error)
    }

    pub fn render_diagnostics(&self) -> String {
        ppl_diag::render_all(&self.diags, &self.sm)
    }
}

/// Runs every implemented front-end stage on `text`; imports are relative to the directory
/// of the file `name` names.
pub fn check_source(name: impl Into<String>, text: impl Into<String>) -> Checked {
    let name = name.into();
    let dir = Path::new(&name)
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default();
    check_source_in(name, text, &dir)
}

/// Like [`check_source`], with imports relative to `dir`.
pub fn check_source_in(name: impl Into<String>, text: impl Into<String>, dir: &Path) -> Checked {
    check_source_with(name, text, dir, BuildOptions::default())
}

/// Like [`check_source_in`], for a build with these options (comptime code sees the mode).
/// `dir` is also the package root for `comptime.readFile`.
pub fn check_source_with(
    name: impl Into<String>,
    text: impl Into<String>,
    dir: &Path,
    opts: BuildOptions,
) -> Checked {
    check(name.into(), text.into(), dir, opts, false)
}

/// The front end for the language server's index in code with errors: resolves and type
/// checks whatever parsed, past errors (each stage marks what it couldn't make sense of as an
/// error node or `Ty::Error`), and stops before MIR. Its diagnostics include follow-up
/// errors the normal pipeline doesn't report: show [`check_source_with`]'s instead.
pub fn check_source_recovering(
    name: impl Into<String>,
    text: impl Into<String>,
    dir: &Path,
    opts: BuildOptions,
) -> Checked {
    check(name.into(), text.into(), dir, opts, true)
}

fn check(name: String, text: String, dir: &Path, opts: BuildOptions, recover: bool) -> Checked {
    let mut sm = SourceMap::new();
    let file = sm.add(name, text);
    let (mut module, mut diags) = ppl_syntax::parse_module(file, &sm.file(file).text);
    // The prelude's declarations (`Error`, …) come first, then the imported modules
    // (dependencies first), then the entry module.
    let mut stmts = Vec::new();
    let mut prelude_files = std::collections::HashSet::new();
    for (name, text) in PRELUDE {
        let pf = sm.add(*name, *text);
        prelude_files.insert(pf);
        let (prelude, mut d) = ppl_syntax::parse_module(pf, text);
        diags.append(&mut d);
        stmts.extend(prelude.stmts);
    }
    let span = module.span;
    let entry = std::mem::replace(
        &mut module,
        Module {
            stmts: Vec::new(),
            span,
        },
    );
    let target_os = comptime_config(dir, &opts).os;
    let mut loaded = modules::load(&mut sm, file, entry, dir, &target_os);
    diags.append(&mut loaded.diags);
    stmts.append(&mut loaded.stmts);
    module.stmts = stmts;
    let mut modules = loaded.modules;
    modules.prelude = prelude_files;
    let mut hir = None;
    let mut types = None;
    let mut mir = None;
    let mut comptime_output = String::new();
    // Decorators run first: they change the declarations the rest compiles.
    let mut expansion = decorators::Expansion::default();
    let cfg = comptime_config(dir, &opts);
    if !diags.iter().any(Diagnostic::is_error) {
        expansion = decorators::expand(&mut sm, &mut module.stmts, &mut modules, file, &cfg);
        diags.append(&mut expansion.diags);
        comptime_output = std::mem::take(&mut expansion.log);
    }
    if recover || !diags.iter().any(Diagnostic::is_error) {
        let (program, mut d) = ppl_sema::resolve_modules(&module, file, &modules);
        diags.append(&mut d);
        if recover || !diags.iter().any(Diagnostic::is_error) {
            let (t, mut d) = ppl_sema::check(&program);
            diags.append(&mut d);
            if !recover && !diags.iter().any(Diagnostic::is_error) {
                let (mut m, mut d) = ppl_mir::lower(&program, &t, sm.file(file), file);
                diags.append(&mut d);
                if !diags.iter().any(Diagnostic::is_error) {
                    let (mut d, out) = ppl_interp::comptime::run(&mut m, &cfg);
                    diags.append(&mut d);
                    comptime_output.push_str(&out);
                    // The comptime values fold further.
                    if !diags.iter().any(Diagnostic::is_error) {
                        diags.append(&mut ppl_mir::fold::fold_program(&mut m));
                    }
                }
                mir = Some(m);
            }
            types = Some(t);
        }
        hir = Some(program);
    }
    expansion.annotate(&mut diags);
    // Report in source order, whichever pass found each problem.
    diags.sort_by_key(|d| d.main_span().map(|s| (s.file, s.start)));
    Checked {
        sm,
        module,
        hir,
        types,
        mir,
        diags,
        comptime_output,
    }
}

/// Declarations every program sees: the core library, written in ppl (`stdlib/core`).
const PRELUDE: &[(&str, &str)] = &[
    (
        "<core>/error.ppl",
        include_str!("../../../stdlib/core/error.ppl"),
    ),
    (
        "<core>/array.ppl",
        include_str!("../../../stdlib/core/array.ppl"),
    ),
    (
        "<core>/string.ppl",
        include_str!("../../../stdlib/core/string.ppl"),
    ),
    (
        "<core>/map.ppl",
        include_str!("../../../stdlib/core/map.ppl"),
    ),
    (
        "<core>/iterator.ppl",
        include_str!("../../../stdlib/core/iterator.ppl"),
    ),
    (
        "<core>/comptime.ppl",
        include_str!("../../../stdlib/core/comptime.ppl"),
    ),
    (
        "<core>/meta.ppl",
        include_str!("../../../stdlib/core/meta.ppl"),
    ),
    (
        "<core>/json.ppl",
        include_str!("../../../stdlib/core/json.ppl"),
    ),
    (
        "<core>/reflect.ppl",
        include_str!("../../../stdlib/core/reflect.ppl"),
    ),
    (
        "<core>/derive.ppl",
        include_str!("../../../stdlib/core/derive.ppl"),
    ),
    (
        "<core>/weak.ppl",
        include_str!("../../../stdlib/core/weak.ppl"),
    ),
    (
        "<core>/event.ppl",
        include_str!("../../../stdlib/core/event.ppl"),
    ),
    (
        "<core>/promise.ppl",
        include_str!("../../../stdlib/core/promise.ppl"),
    ),
    (
        "<core>/abort.ppl",
        include_str!("../../../stdlib/core/abort.ppl"),
    ),
    (
        "<core>/worker.ppl",
        include_str!("../../../stdlib/core/worker.ppl"),
    ),
    (
        "<core>/buffer.ppl",
        include_str!("../../../stdlib/core/buffer.ppl"),
    ),
    (
        "<core>/arena.ppl",
        include_str!("../../../stdlib/core/arena.ppl"),
    ),
    (
        "<core>/text.ppl",
        include_str!("../../../stdlib/core/text.ppl"),
    ),
    (
        "<core>/streams.ppl",
        include_str!("../../../stdlib/core/streams.ppl"),
    ),
    (
        "<core>/net.ppl",
        include_str!("../../../stdlib/core/net.ppl"),
    ),
    (
        "<core>/url.ppl",
        include_str!("../../../stdlib/core/url.ppl"),
    ),
    (
        "<core>/fetch.ppl",
        include_str!("../../../stdlib/core/fetch.ppl"),
    ),
    (
        "<core>/encoding.ppl",
        include_str!("../../../stdlib/core/encoding.ppl"),
    ),
    (
        "<core>/websocket.ppl",
        include_str!("../../../stdlib/core/websocket.ppl"),
    ),
    (
        "<core>/regex.ppl",
        include_str!("../../../stdlib/core/regex.ppl"),
    ),
    (
        "<core>/time.ppl",
        include_str!("../../../stdlib/core/time.ppl"),
    ),
    (
        "<core>/math.ppl",
        include_str!("../../../stdlib/core/math.ppl"),
    ),
    (
        "<core>/fmt.ppl",
        include_str!("../../../stdlib/core/fmt.ppl"),
    ),
    (
        "<core>/crypto.ppl",
        include_str!("../../../stdlib/core/crypto.ppl"),
    ),
];

/// The exit code of a program that panicked (spec §7.5).
pub const PANIC_EXIT_CODE: i32 = 101;

/// The exit code of a program whose top level let an exception escape (spec §7.3).
pub const UNCAUGHT_EXIT_CODE: i32 = 1;

/// Runs a checked program in the MIR interpreter, writing its output to `out`. Runs on a
/// thread with a large stack so deep (but bounded) recursion works.
pub fn run_interp(
    mir: &ppl_mir::Program,
    out: &mut (dyn std::io::Write + Send),
) -> Result<(), ppl_interp::Panic> {
    run_interp_in(mir, out, None)
}

/// [`run_interp`] with its own working directory for file-system operations.
pub fn run_interp_in(
    mir: &ppl_mir::Program,
    out: &mut (dyn std::io::Write + Send),
    cwd: Option<std::path::PathBuf>,
) -> Result<(), ppl_interp::Panic> {
    run_interp_with(mir, out, cwd, Vec::new())
}

/// [`run_interp_in`] with the program's arguments (`args` in `std/process`). A program that
/// calls `exit(code)` ends with a [`ppl_interp::Panic`] whose `exit` is set.
pub fn run_interp_with(
    mir: &ppl_mir::Program,
    out: &mut (dyn std::io::Write + Send),
    cwd: Option<std::path::PathBuf>,
    args: Vec<String>,
) -> Result<(), ppl_interp::Panic> {
    std::thread::scope(|s| {
        std::thread::Builder::new()
            .name("ppl-interp".into())
            .stack_size(1 << 30)
            .spawn_scoped(s, || ppl_interp::run_with(mir, out, cwd, args))
            .expect("spawn interpreter thread")
            .join()
            .expect("interpreter thread panicked")
    })
}

/// Settings for native builds.
#[derive(Clone, Copy, Debug, Default)]
pub struct BuildOptions {
    /// `--release`: optimize.
    pub release: bool,
    /// `--target`; `None` is the host.
    pub target: Option<Target>,
}

/// Compiles a checked program to a native executable at `exe`: an object file from LLVM,
/// linked with the C runtime by lld (Linux targets) or, on a macOS host, the system `cc`.
pub fn build_native(checked: &Checked, exe: &Path, opts: BuildOptions) -> Result<(), String> {
    let mir = checked.mir.as_ref().ok_or("the program has errors")?;
    if let Some(dir) = exe.parent()
        && !dir.as_os_str().is_empty()
    {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    let target = opts.target.unwrap_or_else(Target::host);
    // (A macOS host still links its own programs with `cc`: lld is optional there.)
    let lld = matches!(
        target.os,
        target::Os::Linux | target::Os::Windows | target::Os::Wasi | target::Os::Browser
    ) || (target.os == target::Os::Macos && !target.is_host());
    if !lld && !target.is_host() {
        return Err(format!("building for {target} isn't supported yet"));
    }
    if target.os == target::Os::Browser && browser::wasmgc_selected() {
        return browser::build_gc(mir, exe, &js_packages(&checked.sm));
    }
    let obj = exe.with_extension("o");
    {
        // (wasm code generation sets LLVM options that a concurrent lld link would reset.)
        let _no_links = (target.arch == target::Arch::Wasm32).then(ppl_lld::lock);
        ppl_codegen::emit_object(
            mir,
            &obj,
            ppl_codegen::Options {
                optimize: opts.release,
                triple: (!target.is_host()).then(|| target.clang_triple()),
            },
        )?;
    }
    if lld {
        let r = if target.os == target::Os::Windows {
            link::link_windows(target, &obj, exe)
        } else if target.os == target::Os::Macos {
            link::link_macos(target, &obj, exe, !opts.release)
        } else if target.os == target::Os::Wasi {
            link::link_wasi(target, &obj, exe)
        } else if target.os == target::Os::Browser {
            browser::build(mir, &obj, exe, &js_packages(&checked.sm))
        } else {
            link::link_linux(target, &obj, exe, !opts.release, &mir.links)
        };
        let _ = std::fs::remove_file(&obj);
        return r;
    }
    let mut cmd = std::process::Command::new("cc");
    cmd.arg(&obj)
        .arg(ppl_runtime::static_lib())
        .args(ppl_runtime::crypto_libs())
        .args(ppl_runtime::host_link_flags())
        .args(mir.links.iter().map(|l| format!("-l{l}")));
    if !opts.release {
        // Panic traces name frames through the dynamic symbol table.
        cmd.arg("-Wl,-export_dynamic");
    }
    cmd.arg("-o").arg(exe);
    let out = cmd
        .output()
        .map_err(|e| format!("cannot run the linker (`cc`): {e}"))?;
    let _ = std::fs::remove_file(&obj);
    if !out.status.success() {
        return Err(format!(
            "linking failed:\n{}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(())
}

/// The npm packages a program's `js:npm/<package>` imports name (the glue imports them).
fn js_packages(sm: &ppl_diag::SourceMap) -> Vec<String> {
    sm.ids()
        .filter_map(|f| {
            let name = &sm.file(f).name;
            name.strip_prefix("<js:npm/")?
                .strip_suffix('>')
                .map(str::to_string)
        })
        .collect()
}

/// The most files [`project_files`] lists.
const MAX_PROJECT_FILES: usize = 2000;

/// The nearest directory with a `project.json` above `doc` (else `doc`'s directory).
pub fn project_root(doc: &Path) -> PathBuf {
    let dir = doc.parent().unwrap_or(Path::new("."));
    dir.ancestors()
        .find(|d| d.join("project.json").is_file())
        .unwrap_or(dir)
        .to_path_buf()
}

/// The `.ppl` files under `root` (no hidden directories, `target` or `node_modules`).
pub fn project_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut dirs = vec![root.to_path_buf()];
    while let Some(d) = dirs.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        let mut entries: Vec<PathBuf> = entries.filter_map(|e| Some(e.ok()?.path())).collect();
        entries.sort();
        for p in entries {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if p.is_dir() {
                if !name.starts_with('.') && name != "target" && name != "node_modules" {
                    dirs.push(p);
                }
            } else if name.ends_with(".ppl") && out.len() < MAX_PROJECT_FILES {
                out.push(p);
            }
        }
    }
    out
}

/// The formatter's options for files in `dir`: `project.json`'s `"fmt"` (spec §14.4), found
/// in `dir` or above, over the defaults.
pub fn fmt_options(dir: &Path) -> ppl_syntax::fmt::FmtOptions {
    let mut o = ppl_syntax::fmt::FmtOptions::default();
    let mut d = dir.canonicalize().ok();
    while let Some(cur) = d {
        let p = cur.join("project.json");
        if let Ok(text) = std::fs::read_to_string(&p) {
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text)
                && let Some(f) = json.get("fmt")
            {
                if let Some(n) = f.get("indent").and_then(|v| v.as_u64()) {
                    o.indent = n as usize;
                }
                if let Some(n) = f.get("lineWidth").and_then(|v| v.as_u64()) {
                    o.line_width = n as usize;
                }
                if f.get("quotes").and_then(|v| v.as_str()) == Some("single") {
                    o.quotes = ppl_syntax::fmt::Quotes::Single;
                }
                if let Some(b) = f.get("semicolons").and_then(|v| v.as_bool()) {
                    o.semicolons = b;
                }
            }
            break;
        }
        d = cur.parent().map(Path::to_path_buf);
    }
    o
}

/// Builds `out` (`.wasm` and `.js` glue) with the WasmGC backend (P8.7, decision 264), as
/// `PPL_BACKEND=wasmgc` does for `--target wasm32-browser`.
pub fn build_wasmgc(checked: &Checked, out: &Path) -> Result<(), String> {
    let mir = checked.mir.as_ref().ok_or("the program has errors")?;
    browser::build_gc(mir, out, &js_packages(&checked.sm))
}

/// `ppl build --lib static|shared` (spec §12.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LibKind {
    Static,
    Shared,
}

/// Builds a library for C programs at `out` (`libx.a` or `libx.so`) and its header (`out`
/// with `.h`): the `@export("c")` functions, each starting the calling thread's module
/// instance the first time (spec §12.4, decision 263). Returns the header's path.
pub fn build_library(
    checked: &Checked,
    out: &Path,
    opts: BuildOptions,
    kind: LibKind,
) -> Result<PathBuf, String> {
    let mir = checked.mir.as_ref().ok_or("the program has errors")?;
    let target = opts.target.unwrap_or_else(Target::host);
    let exports: Vec<String> = mir
        .exports
        .iter()
        .filter(|e| e.abi == ppl_sema::hir::ExportAbi::C)
        .map(|e| e.name.clone())
        .collect();
    if exports.is_empty() {
        return Err("a library needs `@export(\"c\")` functions".into());
    }
    if let Some(dir) = out.parent()
        && !dir.as_os_str().is_empty()
    {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    let obj = out.with_extension("o");
    ppl_codegen::emit_object(
        mir,
        &obj,
        ppl_codegen::Options {
            optimize: opts.release,
            triple: (!target.is_host()).then(|| target.clang_triple()),
        },
    )?;
    let r = match kind {
        LibKind::Static => link::link_static_lib(target, &obj, out),
        LibKind::Shared => link::link_shared_lib(target, &obj, out, &exports, &mir.links),
    };
    let _ = std::fs::remove_file(&obj);
    r?;
    let header = out.with_extension("h");
    let guard = header
        .file_stem()
        .map(|s| s.to_string_lossy().to_uppercase())
        .unwrap_or_default();
    std::fs::write(&header, clib::header(mir, &guard))
        .map_err(|e| format!("cannot write {}: {e}", header.display()))?;
    Ok(header)
}

/// Where `ppl run` puts the executable for a source file: a cache directory keyed by the
/// file's path and contents, so unchanged programs aren't rebuilt.
pub fn cached_executable(checked: &Checked, opts: BuildOptions) -> PathBuf {
    use std::hash::{Hash, Hasher};
    let file = checked.sm.file(ppl_diag::FileId(0));
    let mut h = std::collections::hash_map::DefaultHasher::new();
    file.name.hash(&mut h);
    // The whole program as compiled: imported modules and comptime values included.
    match &checked.mir {
        Some(m) => ppl_mir::dump(m).hash(&mut h),
        None => file.text.hash(&mut h),
    }
    opts.release.hash(&mut h);
    opts.target.hash(&mut h);
    std::env::var("PPL_BACKEND").ok().hash(&mut h);
    env!("CARGO_PKG_VERSION").hash(&mut h);
    // The compiler itself: a rebuilt compiler must not reuse old executables.
    if let Ok(meta) = std::env::current_exe().and_then(std::fs::metadata) {
        meta.len().hash(&mut h);
        if let Ok(t) = meta.modified() {
            t.hash(&mut h);
        }
    }
    let mut stem = Path::new(&file.name)
        .file_stem()
        .map_or("main".into(), |s| s.to_string_lossy().into_owned());
    if opts.target.is_some_and(|t| t.os == target::Os::Windows) {
        stem.push_str(".exe");
    }
    if opts.target.is_some_and(|t| t.os == target::Os::Browser) {
        // (The glue; the module is next to it.)
        stem.push_str(".js");
    }
    std::env::temp_dir()
        .join("ppl-cache")
        .join(format!("{:016x}", h.finish()))
        .join(stem)
}

/// Builds (unless cached) and returns the executable for `ppl run`.
pub fn build_cached(checked: &Checked, opts: BuildOptions) -> Result<PathBuf, String> {
    let exe = cached_executable(checked, opts);
    if !exe.is_file() {
        build_native(checked, &exe, opts)?;
    }
    Ok(exe)
}

/// What comptime code sees of the build (`target`, `build`): the host, or `opts.target`.
fn comptime_config(dir: &Path, opts: &BuildOptions) -> ppl_interp::comptime::Config {
    let mut cfg = ppl_interp::comptime::Config::host(dir.to_path_buf(), opts.release);
    if let Some(t) = opts.target {
        use target::{Arch, Os};
        cfg.os = match t.os {
            Os::Linux => "linux",
            Os::Macos => "macos",
            Os::Windows => "windows",
            Os::Wasi => "wasi",
            Os::Browser => "browser",
        }
        .into();
        cfg.arch = match t.arch {
            Arch::X86_64 => "x86_64",
            Arch::Aarch64 => "aarch64",
            Arch::Wasm32 => "wasm32",
        }
        .into();
        cfg.triple = t.llvm_triple().into();
    }
    cfg
}

/// Reads `path` and runs [`check_source`] on it, naming the file by its path.
pub fn check_file(path: &Path) -> std::io::Result<Checked> {
    check_file_with(path, BuildOptions::default())
}

/// [`check_file`] for a build with these options.
pub fn check_file_with(path: &Path, opts: BuildOptions) -> std::io::Result<Checked> {
    let text = std::fs::read_to_string(path)?;
    let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
    Ok(check_source_with(
        path.display().to_string(),
        text,
        &dir,
        opts,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ppl_diag::codes;

    #[test]
    fn stages_of_codes() {
        assert_eq!(Stage::of(codes::UNTERMINATED_STRING), Stage::Parse);
        assert_eq!(Stage::of(codes::UNEXPECTED_TOKEN), Stage::Parse);
        assert_eq!(Stage::of(codes::MISMATCHED_TYPES), Stage::Typecheck);
        assert!(Stage::Parse.is_implemented());
        assert!(Stage::Resolve.is_implemented());
        assert!(Stage::Typecheck.is_implemented());
    }

    #[test]
    fn check_reports_syntax_errors() {
        let c = check_source("t.ppl", "const = 1\n");
        assert!(c.has_errors());
        assert!(c.render_diagnostics().contains("t.ppl:1:7"));
        assert!(!check_source("ok.ppl", "const a = 1\n").has_errors());
    }

    #[test]
    fn comptime_output_is_the_compile_time_log() {
        let src = "comptime {\n  println(`mode ${build.mode}`)\n}\nprintln(\"run\")\n";
        let debug = check_source("t.ppl", src);
        assert!(!debug.has_errors(), "{}", debug.render_diagnostics());
        assert_eq!(debug.comptime_output, "mode debug\n");
        let release = check_source_with(
            "t.ppl",
            src,
            Path::new(""),
            BuildOptions {
                release: true,
                ..Default::default()
            },
        );
        assert_eq!(release.comptime_output, "mode release\n");
    }
}
