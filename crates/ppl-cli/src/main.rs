//! The `ppl` command-line tool.

use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// The padcom programming language toolchain.
#[derive(Parser)]
#[command(name = "ppl", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Compile a program to a native executable.
    Build {
        /// The entry `.ppl` file.
        file: PathBuf,
        /// Where to write the executable (default: the file name without `.ppl`).
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Optimize.
        #[arg(long)]
        release: bool,
        /// Print an intermediate form instead of building: `mir` or `llvm-ir`.
        #[arg(long, value_name = "FORM")]
        emit: Option<String>,
        /// The target: an LLVM triple or an alias such as `linux-x64` (default: the host).
        #[arg(long, value_name = "TARGET", value_parser = ppl_driver::Target::parse)]
        target: Option<ppl_driver::Target>,
        /// Build a library for C programs instead (`static` or `shared`; spec §12.4): its
        /// `@export("c")` functions, and a header next to it.
        #[arg(long, value_name = "KIND", value_parser = parse_lib)]
        lib: Option<ppl_driver::LibKind>,
    },
    /// Compile and run a program.
    Run {
        /// The entry `.ppl` file.
        file: PathBuf,
        /// Run in the MIR interpreter instead of building a native binary.
        #[arg(long)]
        interp: bool,
        /// Optimize.
        #[arg(long)]
        release: bool,
        /// Arguments for the program.
        #[arg(last = true)]
        args: Vec<String>,
    },
    /// Format `.ppl` files in place (spec §14.4; options from `project.json`'s `fmt`).
    Fmt {
        /// Files or directories (default: the current directory).
        paths: Vec<PathBuf>,
        /// Change nothing: list the files that aren't formatted, and fail if there are any.
        #[arg(long)]
        check: bool,
    },
    /// Run the tests in `*.test.ppl` files (spec §14.3), each file in a process of its own,
    /// in parallel.
    Test {
        /// Files or directories to search (default: the current directory).
        paths: Vec<PathBuf>,
        /// Only tests whose full name (`group > test`) contains this.
        #[arg(short = 't', long)]
        filter: Option<String>,
        /// Run in the MIR interpreter instead of native binaries.
        #[arg(long)]
        interp: bool,
        /// Optimize.
        #[arg(long)]
        release: bool,
        /// Run again whenever a `.ppl` file under the paths changes.
        #[arg(long)]
        watch: bool,
        /// How many files run at once (default: the number of CPUs).
        #[arg(short, long)]
        jobs: Option<usize>,
    },
    /// Run the benchmarks (`bench(…)` from `std/test`) in `*.bench.ppl` and `*.test.ppl`
    /// files, one file at a time, optimized.
    Bench {
        /// Files or directories to search (default: the current directory).
        paths: Vec<PathBuf>,
        /// Run in the MIR interpreter instead of native binaries.
        #[arg(long)]
        interp: bool,
    },
    /// The language server (LSP over stdio; editors/vscode starts it).
    Lsp {
        /// Accepted for clients that pass it (stdio is the only transport).
        #[arg(long)]
        stdio: bool,
    },
    /// Parse a file and report syntax errors.
    Parse {
        /// The `.ppl` file to parse.
        file: PathBuf,
        /// Print the syntax tree as S-expressions.
        #[arg(long)]
        dump: bool,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Some(Command::Parse { file, dump }) => parse(&file, dump),
        Some(Command::Build {
            file,
            output,
            release,
            emit,
            target,
            lib: Some(kind),
        }) if emit.is_none() => build_lib(
            &file,
            output,
            ppl_driver::BuildOptions { release, target },
            kind,
        ),
        Some(Command::Build {
            file,
            output,
            release,
            emit,
            target,
            ..
        }) => build(
            &file,
            output,
            ppl_driver::BuildOptions { release, target },
            emit.as_deref(),
        ),
        Some(Command::Fmt { paths, check }) => fmt(&paths, check),
        Some(Command::Lsp { .. }) => match ppl_lsp::run_stdio() {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::FAILURE
            }
        },
        Some(Command::Run {
            file,
            interp,
            release,
            args,
        }) => run(
            &file,
            interp,
            ppl_driver::BuildOptions {
                release,
                ..Default::default()
            },
            &args,
        ),
        Some(Command::Test {
            paths,
            filter,
            interp,
            release,
            watch,
            jobs,
        }) => {
            let t = TestRun {
                paths: if paths.is_empty() {
                    vec![PathBuf::from(".")]
                } else {
                    paths
                },
                filter,
                interp,
                release,
                bench: false,
                jobs: jobs.unwrap_or_else(|| {
                    std::thread::available_parallelism().map_or(4, std::num::NonZero::get)
                }),
            };
            if watch { t.watch() } else { t.run() }
        }
        Some(Command::Bench { paths, interp }) => TestRun {
            paths: if paths.is_empty() {
                vec![PathBuf::from(".")]
            } else {
                paths
            },
            filter: None,
            interp,
            release: true,
            bench: true,
            jobs: 1,
        }
        .run(),
        None => ExitCode::SUCCESS,
    }
}

/// `ppl test` / `ppl bench`: finds the files and runs each through `ppl run` in a child
/// process (isolated: a crash or `exit` ends only that file), collecting their reports.
struct TestRun {
    paths: Vec<PathBuf>,
    filter: Option<String>,
    interp: bool,
    release: bool,
    bench: bool,
    jobs: usize,
}

/// One file's result.
struct FileResult {
    file: PathBuf,
    output: String,
    /// Passed, failed, skipped (or benchmarks run); `None` if the file didn't finish.
    counts: Option<(u64, u64, u64)>,
    ok: bool,
}

impl TestRun {
    fn wanted(&self, name: &str) -> bool {
        name.ends_with(".test.ppl") || (self.bench && name.ends_with(".bench.ppl"))
    }

    /// The test files under the paths, sorted (hidden directories and `target` skipped).
    fn files(&self) -> Vec<PathBuf> {
        fn walk(t: &TestRun, dir: &Path, out: &mut Vec<PathBuf>) {
            let Ok(rd) = std::fs::read_dir(dir) else {
                return;
            };
            for e in rd.flatten() {
                let p = e.path();
                let name = e.file_name().to_string_lossy().into_owned();
                if p.is_dir() {
                    if !name.starts_with('.') && name != "target" && name != "node_modules" {
                        walk(t, &p, out);
                    }
                } else if t.wanted(&name) {
                    out.push(p);
                }
            }
        }
        let mut out = Vec::new();
        for p in &self.paths {
            if p.is_dir() {
                walk(self, p, &mut out);
            } else {
                out.push(p.clone());
            }
        }
        out.sort();
        out.dedup();
        out
    }

    fn run_file(&self, file: &Path) -> FileResult {
        let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("ppl"));
        let mut cmd = std::process::Command::new(exe);
        cmd.arg("run");
        if self.interp {
            cmd.arg("--interp");
        }
        if self.release {
            cmd.arg("--release");
        }
        cmd.arg(file);
        cmd.env("PPL_TEST_MODE", if self.bench { "bench" } else { "test" });
        match &self.filter {
            Some(f) => cmd.env("PPL_TEST_FILTER", f),
            None => cmd.env_remove("PPL_TEST_FILTER"),
        };
        let out = match cmd.output() {
            Ok(o) => o,
            Err(e) => {
                return FileResult {
                    file: file.to_path_buf(),
                    output: format!("cannot run: {e}\n"),
                    counts: None,
                    ok: false,
                };
            }
        };
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        let mut counts = None;
        let mut output = String::new();
        for line in stdout.lines() {
            if let Some(rest) = line.strip_prefix("ppl-test: ") {
                // "N passed, M failed, K skipped"
                let n: Vec<u64> = rest
                    .split(", ")
                    .filter_map(|p| p.split(' ').next()?.parse().ok())
                    .collect();
                if let [a, b, c] = n[..] {
                    counts = Some((a, b, c));
                }
            } else if let Some(rest) = line.strip_prefix("ppl-bench: ") {
                let n = rest
                    .split(' ')
                    .next()
                    .and_then(|x| x.parse().ok())
                    .unwrap_or(0);
                counts = Some((n, 0, 0));
            } else {
                output.push_str(line);
                output.push('\n');
            }
        }
        output.push_str(&stderr);
        let ok = counts.is_some_and(|c| c.1 == 0) && (out.status.success() || counts.is_some());
        if counts.is_none() && !self.bench {
            // A crash, a compile error, or a file without tests.
            output.push_str(&format!(
                "(the file didn't finish: exit status {})\n",
                out.status.code().map_or("signal".into(), |c| c.to_string())
            ));
        }
        FileResult {
            file: file.to_path_buf(),
            output,
            counts,
            ok: ok || (self.bench && out.status.success()),
        }
    }

    fn run(&self) -> ExitCode {
        let files = self.files();
        if files.is_empty() {
            eprintln!(
                "no {} files found",
                if self.bench {
                    "*.bench.ppl or *.test.ppl"
                } else {
                    "*.test.ppl"
                }
            );
            return ExitCode::FAILURE;
        }
        let next = std::sync::atomic::AtomicUsize::new(0);
        let results: std::sync::Mutex<Vec<FileResult>> = std::sync::Mutex::new(Vec::new());
        let print = std::sync::Mutex::new(());
        std::thread::scope(|s| {
            for _ in 0..self.jobs.max(1).min(files.len()) {
                s.spawn(|| {
                    loop {
                        let i = next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        let Some(f) = files.get(i) else { break };
                        let r = self.run_file(f);
                        {
                            let _g = print.lock().expect("print lock");
                            let mark = if r.ok { "✓" } else { "✗" };
                            println!("{mark} {}", r.file.display());
                            print!("{}", r.output);
                        }
                        results.lock().expect("results").push(r);
                    }
                });
            }
        });
        let results = results.into_inner().expect("results");
        let failed_files = results.iter().filter(|r| !r.ok).count();
        let (mut p, mut f, mut k) = (0, 0, 0);
        for r in &results {
            if let Some((a, b, c)) = r.counts {
                p += a;
                f += b;
                k += c;
            }
        }
        if self.bench {
            println!("\n{} files, {p} benchmarks", results.len());
        } else {
            println!(
                "\nfiles: {} passed, {failed_files} failed | tests: {p} passed, {f} failed, {k} skipped",
                results.len() - failed_files
            );
        }
        if failed_files == 0 {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        }
    }

    /// Runs, then again after each change to a `.ppl` file under the paths (polled).
    fn watch(&self) -> ExitCode {
        fn stamp(paths: &[PathBuf]) -> Vec<(PathBuf, std::time::SystemTime)> {
            fn walk(dir: &Path, out: &mut Vec<(PathBuf, std::time::SystemTime)>) {
                let Ok(rd) = std::fs::read_dir(dir) else {
                    return;
                };
                for e in rd.flatten() {
                    let p = e.path();
                    let name = e.file_name().to_string_lossy().into_owned();
                    if p.is_dir() {
                        if !name.starts_with('.') && name != "target" {
                            walk(&p, out);
                        }
                    } else if name.ends_with(".ppl")
                        && let Ok(t) = e.metadata().and_then(|m| m.modified())
                    {
                        out.push((p, t));
                    }
                }
            }
            let mut out = Vec::new();
            for p in paths {
                if p.is_dir() {
                    walk(p, &mut out);
                } else if let Ok(t) = std::fs::metadata(p).and_then(|m| m.modified()) {
                    out.push((p.clone(), t));
                }
            }
            out.sort();
            out
        }
        loop {
            let before = stamp(&self.paths);
            self.run();
            println!("\nwatching for changes (ctrl-c to stop)…");
            while stamp(&self.paths) == before {
                std::thread::sleep(std::time::Duration::from_millis(300));
            }
            println!();
        }
    }
}

fn load(path: &Path, opts: ppl_driver::BuildOptions) -> Option<ppl_driver::Checked> {
    // `ppl_driver::check_file` names the file by its path, which then appears in diagnostics.
    match ppl_driver::check_file_with(path, opts) {
        Ok(c) => Some(c),
        Err(e) => {
            eprintln!("error: cannot read {}: {e}", path.display());
            None
        }
    }
}

fn checked_or_report(
    path: &Path,
    opts: ppl_driver::BuildOptions,
) -> Result<ppl_driver::Checked, ExitCode> {
    let Some(checked) = load(path, opts) else {
        return Err(ExitCode::from(2));
    };
    // The compile-time log.
    eprint!("{}", checked.comptime_output);
    // Errors, and warnings even when there are none.
    if !checked.diags.is_empty() {
        eprint!("{}", checked.render_diagnostics());
    }
    if checked.has_errors() {
        return Err(ExitCode::FAILURE);
    }
    Ok(checked)
}

fn run(path: &Path, interp: bool, opts: ppl_driver::BuildOptions, args: &[String]) -> ExitCode {
    let checked = match checked_or_report(path, opts) {
        Ok(c) => c,
        Err(code) => return code,
    };
    if interp {
        let mir = checked.mir.as_ref().expect("MIR after a clean check");
        let mut out = std::io::stdout();
        return match ppl_driver::run_interp_with(mir, &mut out, None, args.to_vec()) {
            Ok(()) => ExitCode::SUCCESS,
            Err(p) if p.exit.is_some() => ExitCode::from(p.exit.unwrap_or(0).clamp(0, 255) as u8),
            Err(p) => {
                eprint!("{}", p.report());
                ExitCode::from(if p.uncaught {
                    ppl_driver::UNCAUGHT_EXIT_CODE
                } else {
                    ppl_driver::PANIC_EXIT_CODE
                } as u8)
            }
        };
    }
    let exe = match ppl_driver::build_cached(&checked, opts) {
        Ok(exe) => exe,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };
    match std::process::Command::new(&exe).args(args).status() {
        Ok(status) => ExitCode::from(status.code().unwrap_or(1).clamp(0, 255) as u8),
        Err(e) => {
            eprintln!("error: cannot run {}: {e}", exe.display());
            ExitCode::FAILURE
        }
    }
}

/// `ppl fmt [--check] [paths…]`.
fn fmt(paths: &[PathBuf], check: bool) -> ExitCode {
    let roots: Vec<PathBuf> = if paths.is_empty() {
        vec![PathBuf::from(".")]
    } else {
        paths.to_vec()
    };
    let mut files = Vec::new();
    for r in &roots {
        ppl_files(r, &mut files);
    }
    files.sort();
    let mut unformatted = 0;
    let mut failed = false;
    for f in &files {
        let Ok(text) = std::fs::read_to_string(f) else {
            eprintln!("error: cannot read {}", f.display());
            failed = true;
            continue;
        };
        // (Like Prettier: a file with syntax errors isn't touched.)
        let (_, diags) = ppl_syntax::parse_module(ppl_diag::FileId(0), &text);
        if diags.iter().any(|d| d.is_error()) {
            eprintln!("skipping {}: it has syntax errors", f.display());
            continue;
        }
        let dir = f.parent().unwrap_or(Path::new("."));
        let opts = ppl_driver::fmt_options(dir);
        match ppl_syntax::fmt::format(&text, opts) {
            Ok(out) if out == text => {}
            Ok(out) => {
                unformatted += 1;
                if check {
                    println!("{}", f.display());
                } else if let Err(e) = std::fs::write(f, out) {
                    eprintln!("error: cannot write {}: {e}", f.display());
                    failed = true;
                }
            }
            Err(e) => {
                eprintln!("error: {}: {e}", f.display());
                failed = true;
            }
        }
    }
    if failed || (check && unformatted > 0) {
        if check && unformatted > 0 {
            eprintln!("{unformatted} file(s) aren't formatted (run `ppl fmt`)");
        }
        return ExitCode::FAILURE;
    }
    if !check {
        println!("formatted {unformatted} of {} file(s)", files.len());
    }
    ExitCode::SUCCESS
}

/// The `.ppl` files at `p` (a file, or a directory searched recursively, skipping hidden
/// directories, `target` and `node_modules`).
fn ppl_files(p: &Path, out: &mut Vec<PathBuf>) {
    if p.is_file() {
        if p.extension().is_some_and(|e| e == "ppl") {
            out.push(p.to_path_buf());
        }
        return;
    }
    let Ok(entries) = std::fs::read_dir(p) else {
        return;
    };
    for e in entries.flatten() {
        let path = e.path();
        let name = e.file_name().to_string_lossy().into_owned();
        if path.is_dir() && (name.starts_with('.') || name == "target" || name == "node_modules") {
            continue;
        }
        ppl_files(&path, out);
    }
}

fn parse_lib(s: &str) -> Result<ppl_driver::LibKind, String> {
    match s {
        "static" => Ok(ppl_driver::LibKind::Static),
        "shared" => Ok(ppl_driver::LibKind::Shared),
        _ => Err(format!("`{s}`: expected `static` or `shared`")),
    }
}

/// `ppl build --lib static|shared`: `lib<name>.a`/`.so` and `lib<name>.h` by default.
fn build_lib(
    path: &Path,
    output: Option<PathBuf>,
    opts: ppl_driver::BuildOptions,
    kind: ppl_driver::LibKind,
) -> ExitCode {
    let checked = match checked_or_report(path, opts) {
        Ok(c) => c,
        Err(code) => return code,
    };
    let out = output.unwrap_or_else(|| {
        let stem = path.file_stem().unwrap_or_default().to_string_lossy();
        let ext = match kind {
            ppl_driver::LibKind::Static => "a",
            ppl_driver::LibKind::Shared => "so",
        };
        PathBuf::from(format!("lib{stem}.{ext}"))
    });
    match ppl_driver::build_library(&checked, &out, opts, kind) {
        Ok(header) => {
            println!("built {} and {}", out.display(), header.display());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn build(
    path: &Path,
    output: Option<PathBuf>,
    opts: ppl_driver::BuildOptions,
    emit: Option<&str>,
) -> ExitCode {
    let checked = match checked_or_report(path, opts) {
        Ok(c) => c,
        Err(code) => return code,
    };
    let mir = checked.mir.as_ref().expect("MIR after a clean check");
    match emit {
        Some("mir") => {
            print!("{}", ppl_mir::dump(mir));
            ExitCode::SUCCESS
        }
        // (Without `--target`, the IR is the target-independent form the snapshot tests use.)
        Some("llvm-ir") => match opts.target {
            None => {
                print!("{}", ppl_codegen::emit_llvm_ir(mir));
                ExitCode::SUCCESS
            }
            Some(t) => {
                let o = ppl_codegen::Options {
                    optimize: opts.release,
                    triple: Some(t.llvm_triple()),
                };
                match ppl_codegen::emit_llvm_ir_for(mir, o) {
                    Ok(ir) => {
                        print!("{ir}");
                        ExitCode::SUCCESS
                    }
                    Err(e) => {
                        eprintln!("error: {e}");
                        ExitCode::FAILURE
                    }
                }
            }
        },
        Some(other) => {
            eprintln!("error: unknown --emit form `{other}` (expected `mir` or `llvm-ir`)");
            ExitCode::from(2)
        }
        None => {
            let exe = output.unwrap_or_else(|| PathBuf::from(path.file_stem().unwrap_or_default()));
            match ppl_driver::build_native(&checked, &exe, opts) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("error: {e}");
                    ExitCode::FAILURE
                }
            }
        }
    }
}

fn parse(path: &Path, dump: bool) -> ExitCode {
    let checked = match ppl_driver::check_file(path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: cannot read {}: {e}", path.display());
            return ExitCode::from(2);
        }
    };
    if dump {
        print!("{}", ppl_syntax::dump::dump_module(&checked.module));
    }
    if checked.diags.is_empty() {
        ExitCode::SUCCESS
    } else {
        eprint!("{}", checked.render_diagnostics());
        ExitCode::FAILURE
    }
}
