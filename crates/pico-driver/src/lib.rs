//! Compilation pipeline, caching, targets and linking for pico.
//!
//! [`check_source`] runs the front end: parsing, name resolution, type checking, and lowering
//! to MIR. Later stages plug in
//! there as they're built, and [`Stage::IMPLEMENTED`] tells tools which diagnostics can
//! currently be produced. Each stage runs only if the previous ones reported no errors.

use pico_diag::{Code, Diagnostic, SourceMap};
use pico_syntax::ast::Module;
use std::path::{Path, PathBuf};

mod browser;
mod builtins;
mod cheader;
mod clib;
mod decorators;
mod dts;
pub mod fingerprint;
mod git;
mod glibc;
mod link;
mod mingw;
mod modules;
mod musl;
pub mod packages;
pub mod semver;
pub mod target;
pub use pico_codegen::export_symbol;
/// What the WasmGC backend's "not supported yet" errors say (P8.7).
pub const WASMGC_UNSUPPORTED: &str = pico_codegen::wasmgc::UNSUPPORTED;
/// Whether this compiler links with lld (needed for every target but a macOS host's own).
pub const LLD_AVAILABLE: bool = pico_lld::AVAILABLE;
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

    /// The stage that reports `code` (from the ranges in `pico_diag::codes`).
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
    pub hir: Option<pico_sema::hir::Program>,
    /// Type information, if resolution succeeded.
    pub types: Option<pico_sema::Types>,
    /// The MIR, if type checking succeeded.
    pub mir: Option<pico_mir::Program>,
    pub diags: Vec<Diagnostic>,
    /// What `comptime` code printed (the compile-time log).
    pub comptime_output: String,
    /// Files `comptime.readFile` read (inputs of the build besides the sources).
    pub files_read: Vec<PathBuf>,
}

impl Checked {
    pub fn has_errors(&self) -> bool {
        self.diags.iter().any(Diagnostic::is_error)
    }

    pub fn render_diagnostics(&self) -> String {
        pico_diag::render_all(&self.diags, &self.sm)
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
    check(name.into(), text.into(), dir, opts, Mode::Build)
}

/// Like [`check_source_with`], for the language server: the program's decorator functions
/// stay in it (as ordinary functions, not run), so the index covers their code.
pub fn check_source_for_editor(
    name: impl Into<String>,
    text: impl Into<String>,
    dir: &Path,
    opts: BuildOptions,
) -> Checked {
    check(name.into(), text.into(), dir, opts, Mode::Editor)
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
    check(name.into(), text.into(), dir, opts, Mode::Recovering)
}

/// With `PICO_TIMINGS=1`: how long each front-end stage takes, on stderr.
struct Timings {
    on: bool,
    last: std::time::Instant,
    start: std::time::Instant,
}

impl Timings {
    fn new() -> Timings {
        let now = std::time::Instant::now();
        Timings {
            on: std::env::var_os("PICO_TIMINGS").is_some_and(|v| v == "1"),
            last: now,
            start: now,
        }
    }

    fn stage(&mut self, name: &str) {
        if self.on {
            let now = std::time::Instant::now();
            eprintln!(
                "[timings] {name:<10} {:>8.1} ms",
                (now - self.last).as_secs_f64() * 1000.0
            );
            self.last = now;
        }
    }

    fn total(&self) {
        if self.on {
            eprintln!(
                "[timings] {:<10} {:>8.1} ms",
                "total",
                self.start.elapsed().as_secs_f64() * 1000.0
            );
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Build,
    /// [`check_source_for_editor`].
    Editor,
    /// [`check_source_recovering`] (for the editor too).
    Recovering,
}

fn check(name: String, text: String, dir: &Path, opts: BuildOptions, mode: Mode) -> Checked {
    let recover = mode == Mode::Recovering;
    let editor = mode != Mode::Build;
    let mut t = Timings::new();
    // (Forget reads of an earlier check on this thread.)
    pico_interp::comptime::take_files_read();
    let mut sm = SourceMap::new();
    let file = sm.add(name, text);
    let (mut module, mut diags) = pico_syntax::parse_module(file, &sm.file(file).text);
    // The prelude's declarations (`Error`, …) come first, then the imported modules
    // (dependencies first), then the entry module.
    let mut stmts = Vec::new();
    let mut prelude_files = std::collections::HashSet::new();
    for (name, text) in PRELUDE {
        let pf = sm.add(*name, *text);
        prelude_files.insert(pf);
        let (prelude, mut d) = pico_syntax::parse_module(pf, text);
        diags.append(&mut d);
        stmts.extend(prelude.stmts);
    }
    t.stage("parse");
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
    t.stage("imports");
    let mut hir = None;
    let mut types = None;
    let mut mir = None;
    let mut comptime_output = String::new();
    // Decorators run first: they change the declarations the rest compiles.
    let mut expansion = decorators::Expansion::default();
    let cfg = comptime_config(dir, &opts);
    if !diags.iter().any(Diagnostic::is_error) {
        expansion =
            decorators::expand(&mut sm, &mut module.stmts, &mut modules, file, &cfg, editor);
        diags.append(&mut expansion.diags);
        comptime_output = std::mem::take(&mut expansion.log);
    }
    t.stage("decorators");
    if recover || !diags.iter().any(Diagnostic::is_error) {
        let (program, mut d) = pico_sema::resolve_modules(&module, file, &modules);
        diags.append(&mut d);
        t.stage("resolve");
        if recover || !diags.iter().any(Diagnostic::is_error) {
            let (ty, mut d) = pico_sema::check(&program);
            diags.append(&mut d);
            t.stage("typecheck");
            if !recover && !diags.iter().any(Diagnostic::is_error) {
                let (mut m, mut d) = pico_mir::lower(&program, &ty, sm.file(file), file);
                diags.append(&mut d);
                t.stage("mir");
                if !diags.iter().any(Diagnostic::is_error) {
                    let (mut d, out) = pico_interp::comptime::run(&mut m, &cfg);
                    diags.append(&mut d);
                    comptime_output.push_str(&out);
                    // The comptime values fold further.
                    if !diags.iter().any(Diagnostic::is_error) {
                        diags.append(&mut pico_mir::fold::fold_program(&mut m));
                    }
                    t.stage("comptime");
                }
                m.sources = debug_sources(&sm);
                mir = Some(m);
            }
            types = Some(ty);
        }
        hir = Some(program);
    }
    t.total();
    expansion.annotate(&mut diags);
    // Report in source order, whichever pass found each problem.
    diags.sort_by_key(|d| d.main_span().map(|s| (s.file, s.start)));
    // (Kept decorator functions, in the language server's analysis, are checked twice: in the
    // decorators' program and in this one.)
    if editor {
        let mut seen = std::collections::HashSet::new();
        diags.retain(|d| seen.insert((d.code, d.main_span(), d.message.clone())));
    }
    Checked {
        sm,
        module,
        hir,
        types,
        mir,
        diags,
        comptime_output,
        files_read: pico_interp::comptime::take_files_read(),
    }
}

/// On macOS the linker leaves the DWARF in the object file (the executable only points to it):
/// `dsymutil` copies it into `exe.dSYM` before the object goes, so lldb finds it (spec §14.5),
/// and so does `pico symbolize` for a release build (whose executable is then stripped of the
/// debug map, spec §7.1).
fn keep_macos_debug_info(exe: &Path, release: bool) {
    match std::process::Command::new("dsymutil").arg(exe).output() {
        Ok(out) if !out.status.success() => eprintln!(
            "warning: dsymutil failed, the debugger won't see pico's source lines:\n{}",
            String::from_utf8_lossy(&out.stderr)
        ),
        Ok(_) if release => {
            let _ = std::process::Command::new("strip")
                .arg("-S")
                .arg(exe)
                .output();
        }
        _ => {}
    }
}

/// The source files for debug info, by `FileId`: their paths (the core library's at the bundled
/// stdlib's copy, where the debugger can show it) and line starts.
fn debug_sources(sm: &SourceMap) -> Vec<pico_mir::mir::Source> {
    sm.ids()
        .map(|id| {
            let f = sm.file(id);
            let path = if let Some(rest) = f.name.strip_prefix("<core>/") {
                let p = stdlib_dir().join("core").join(rest);
                p.canonicalize().unwrap_or(p).display().to_string()
            } else if f.name.starts_with('<') {
                f.name.clone()
            } else {
                let p = Path::new(&f.name);
                p.canonicalize()
                    .unwrap_or_else(|_| p.to_path_buf())
                    .display()
                    .to_string()
            };
            pico_mir::mir::Source {
                path,
                line_starts: (0..f.line_count()).map(|l| f.line_start(l)).collect(),
                library: f.name.starts_with("<core>/"),
            }
        })
        .collect()
}

/// Declarations every program sees: the core library, written in pico (`stdlib/core`).
const PRELUDE: &[(&str, &str)] = &[
    (
        "<core>/error.pi",
        include_str!("../../../stdlib/core/error.pi"),
    ),
    (
        "<core>/array.pi",
        include_str!("../../../stdlib/core/array.pi"),
    ),
    (
        "<core>/string.pi",
        include_str!("../../../stdlib/core/string.pi"),
    ),
    ("<core>/map.pi", include_str!("../../../stdlib/core/map.pi")),
    (
        "<core>/iterator.pi",
        include_str!("../../../stdlib/core/iterator.pi"),
    ),
    (
        "<core>/comptime.pi",
        include_str!("../../../stdlib/core/comptime.pi"),
    ),
    (
        "<core>/meta.pi",
        include_str!("../../../stdlib/core/meta.pi"),
    ),
    (
        "<core>/json.pi",
        include_str!("../../../stdlib/core/json.pi"),
    ),
    (
        "<core>/reflect.pi",
        include_str!("../../../stdlib/core/reflect.pi"),
    ),
    (
        "<core>/derive.pi",
        include_str!("../../../stdlib/core/derive.pi"),
    ),
    (
        "<core>/weak.pi",
        include_str!("../../../stdlib/core/weak.pi"),
    ),
    (
        "<core>/event.pi",
        include_str!("../../../stdlib/core/event.pi"),
    ),
    (
        "<core>/promise.pi",
        include_str!("../../../stdlib/core/promise.pi"),
    ),
    (
        "<core>/abort.pi",
        include_str!("../../../stdlib/core/abort.pi"),
    ),
    (
        "<core>/worker.pi",
        include_str!("../../../stdlib/core/worker.pi"),
    ),
    (
        "<core>/buffer.pi",
        include_str!("../../../stdlib/core/buffer.pi"),
    ),
    (
        "<core>/arena.pi",
        include_str!("../../../stdlib/core/arena.pi"),
    ),
    (
        "<core>/text.pi",
        include_str!("../../../stdlib/core/text.pi"),
    ),
    (
        "<core>/streams.pi",
        include_str!("../../../stdlib/core/streams.pi"),
    ),
    ("<core>/net.pi", include_str!("../../../stdlib/core/net.pi")),
    ("<core>/url.pi", include_str!("../../../stdlib/core/url.pi")),
    (
        "<core>/fetch.pi",
        include_str!("../../../stdlib/core/fetch.pi"),
    ),
    (
        "<core>/encoding.pi",
        include_str!("../../../stdlib/core/encoding.pi"),
    ),
    (
        "<core>/websocket.pi",
        include_str!("../../../stdlib/core/websocket.pi"),
    ),
    (
        "<core>/regex.pi",
        include_str!("../../../stdlib/core/regex.pi"),
    ),
    (
        "<core>/time.pi",
        include_str!("../../../stdlib/core/time.pi"),
    ),
    (
        "<core>/math.pi",
        include_str!("../../../stdlib/core/math.pi"),
    ),
    ("<core>/fmt.pi", include_str!("../../../stdlib/core/fmt.pi")),
    (
        "<core>/crypto.pi",
        include_str!("../../../stdlib/core/crypto.pi"),
    ),
];

/// The exit code of a program that panicked (spec §7.5).
pub const PANIC_EXIT_CODE: i32 = 101;

/// The exit code of a program whose top level let an exception escape (spec §7.3).
pub const UNCAUGHT_EXIT_CODE: i32 = 1;

/// Runs a checked program in the MIR interpreter, writing its output to `out`. Runs on a
/// thread with a large stack so deep (but bounded) recursion works.
pub fn run_interp(
    mir: &pico_mir::Program,
    out: &mut (dyn std::io::Write + Send),
) -> Result<(), pico_interp::Panic> {
    run_interp_in(mir, out, None)
}

/// [`run_interp`] with its own working directory for file-system operations.
pub fn run_interp_in(
    mir: &pico_mir::Program,
    out: &mut (dyn std::io::Write + Send),
    cwd: Option<std::path::PathBuf>,
) -> Result<(), pico_interp::Panic> {
    run_interp_with(mir, out, cwd, Vec::new())
}

/// [`run_interp_in`] with the program's arguments (`args` in `std/process`). A program that
/// calls `exit(code)` ends with a [`pico_interp::Panic`] whose `exit` is set.
pub fn run_interp_with(
    mir: &pico_mir::Program,
    out: &mut (dyn std::io::Write + Send),
    cwd: Option<std::path::PathBuf>,
    args: Vec<String>,
) -> Result<(), pico_interp::Panic> {
    std::thread::scope(|s| {
        std::thread::Builder::new()
            .name("pico-interp".into())
            .stack_size(1 << 30)
            .spawn_scoped(s, || pico_interp::run_with(mir, out, cwd, args))
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
        let _no_links = (target.arch == target::Arch::Wasm32).then(pico_lld::lock);
        pico_codegen::emit_object(
            mir,
            &obj,
            pico_codegen::Options {
                optimize: opts.release,
                // (Release builds too: their DWARF goes into the separate debug file that
                // `pico symbolize` reads; not on Windows yet, which would need a PDB. Wasm's
                // isn't wired to a debugger yet.)
                debug: target.arch != target::Arch::Wasm32
                    && (!opts.release || target.os != target::Os::Windows),
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
        if r.is_ok() && target.os == target::Os::Macos {
            keep_macos_debug_info(exe, opts.release);
        }
        let _ = std::fs::remove_file(&obj);
        return r;
    }
    let mut cmd = std::process::Command::new("cc");
    cmd.arg(&obj)
        .arg(pico_runtime::static_lib())
        .args(pico_runtime::crypto_libs())
        .args(pico_runtime::host_link_flags())
        .args(mir.links.iter().map(|l| format!("-l{l}")));
    if !opts.release {
        // Panic traces name frames through the dynamic symbol table.
        cmd.arg("-Wl,-export_dynamic");
    }
    cmd.arg("-o").arg(exe);
    let out = cmd
        .output()
        .map_err(|e| format!("cannot run the linker (`cc`): {e}"))?;
    if out.status.success() {
        keep_macos_debug_info(exe, opts.release);
    }
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
fn js_packages(sm: &pico_diag::SourceMap) -> Vec<String> {
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

/// The `.pi` files under `root` (no hidden directories, `target` or `node_modules`).
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
            } else if name.ends_with(".pi") && out.len() < MAX_PROJECT_FILES {
                out.push(p);
            }
        }
    }
    out
}

/// The formatter's options for files in `dir`: `project.json`'s `"fmt"` (spec §14.4), found
/// in `dir` or above, over the defaults.
pub fn fmt_options(dir: &Path) -> pico_syntax::fmt::FmtOptions {
    let mut o = pico_syntax::fmt::FmtOptions::default();
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
                    o.quotes = pico_syntax::fmt::Quotes::Single;
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
/// `PICO_BACKEND=wasmgc` does for `--target wasm32-browser`.
pub fn build_wasmgc(checked: &Checked, out: &Path) -> Result<(), String> {
    let mir = checked.mir.as_ref().ok_or("the program has errors")?;
    browser::build_gc(mir, out, &js_packages(&checked.sm))
}

/// `pico build --lib static|shared` (spec §12.4).
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
        .filter(|e| e.abi == pico_sema::hir::ExportAbi::C)
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
    pico_codegen::emit_object(
        mir,
        &obj,
        pico_codegen::Options {
            optimize: opts.release,
            debug: !opts.release && target.arch != target::Arch::Wasm32,
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

/// Where `pico run` puts the executable for a source file: a cache directory keyed by the
/// file's path and contents, so unchanged programs aren't rebuilt.
pub fn cached_executable(checked: &Checked, opts: BuildOptions) -> PathBuf {
    use std::hash::{Hash, Hasher};
    let file = checked.sm.file(pico_diag::FileId(0));
    let mut h = std::collections::hash_map::DefaultHasher::new();
    file.name.hash(&mut h);
    // The whole program as compiled: imported modules and comptime values included.
    match &checked.mir {
        Some(m) => pico_mir::dump(m).hash(&mut h),
        None => file.text.hash(&mut h),
    }
    opts.release.hash(&mut h);
    opts.target.hash(&mut h);
    std::env::var("PICO_BACKEND").ok().hash(&mut h);
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
        .join("pico-cache")
        .join(format!("{:016x}", h.finish()))
        .join(stem)
}

/// Builds (unless cached) and returns the executable for `pico run`.
pub fn build_cached(checked: &Checked, opts: BuildOptions) -> Result<PathBuf, String> {
    let exe = cached_executable(checked, opts);
    if !exe.is_file() {
        build_native(checked, &exe, opts)?;
    }
    Ok(exe)
}

/// What comptime code sees of the build (`target`, `build`): the host, or `opts.target`.
fn comptime_config(dir: &Path, opts: &BuildOptions) -> pico_interp::comptime::Config {
    let mut cfg = pico_interp::comptime::Config::host(dir.to_path_buf(), opts.release);
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
    use pico_diag::codes;

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
        let c = check_source("t.pi", "const = 1\n");
        assert!(c.has_errors());
        assert!(c.render_diagnostics().contains("t.pi:1:7"));
        assert!(!check_source("ok.pi", "const a = 1\n").has_errors());
    }

    #[test]
    fn comptime_output_is_the_compile_time_log() {
        let src = "comptime {\n  println(`mode ${build.mode}`)\n}\nprintln(\"run\")\n";
        let debug = check_source("t.pi", src);
        assert!(!debug.has_errors(), "{}", debug.render_diagnostics());
        assert_eq!(debug.comptime_output, "mode debug\n");
        let release = check_source_with(
            "t.pi",
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
