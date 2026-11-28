//! The `pico` command-line tool.

mod dap;
mod doc;
mod symbolize;

use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// The Pico Programming Language toolchain.
#[derive(Parser)]
#[command(name = "pico", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Compile a program to a native executable.
    Build {
        /// The entry `.pi` file (default: the project's `entry`, from `project.json`).
        file: Option<PathBuf>,
        /// Where to write the executable (default: the file name without `.pi`).
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Optimize.
        #[arg(long)]
        release: bool,
        /// Print an intermediate form instead of building: `mir` or `llvm-ir`.
        #[arg(long, value_name = "FORM")]
        emit: Option<String>,
        /// The target: an LLVM triple or an alias such as `linux-x64` (default: the host).
        #[arg(long, value_name = "TARGET", value_parser = pico_driver::Target::parse)]
        target: Option<pico_driver::Target>,
        /// Build a library for C programs instead (`static` or `shared`; spec §12.4): its
        /// `@export("c")` functions, and a header next to it.
        #[arg(long, value_name = "KIND", value_parser = parse_lib)]
        lib: Option<pico_driver::LibKind>,
        /// Build even if nothing changed since the last build (spec §14.6).
        #[arg(long)]
        force: bool,
        /// A directory to look for `@link` libraries in, before project.json's `c.libDirs`
        /// and the system's (spec §12.1). May be given several times.
        #[arg(short = 'L', long = "lib-dir", value_name = "DIR")]
        lib_dirs: Vec<PathBuf>,
    },
    /// Compile and run a program.
    Run {
        /// The entry `.pi` file (default: the project's `entry`, from `project.json`).
        file: Option<PathBuf>,
        /// Run in the MIR interpreter instead of building a native binary.
        #[arg(long)]
        interp: bool,
        /// Optimize.
        #[arg(long)]
        release: bool,
        /// A directory to look for `@link` libraries in (as for `pico build`).
        #[arg(short = 'L', long = "lib-dir", value_name = "DIR")]
        lib_dirs: Vec<PathBuf>,
        /// Build for this target and run it here (#77): `wasm32-wasi` under wasmtime,
        /// `wasm32-browser` under Node.js, another Linux architecture through qemu-user.
        #[arg(long, value_name = "TARGET", value_parser = pico_driver::Target::parse)]
        target: Option<pico_driver::Target>,
        /// Arguments for the program.
        #[arg(last = true)]
        args: Vec<String>,
    },
    /// Format `.pi` files in place (spec §14.4; options from `project.json`'s `fmt`).
    Fmt {
        /// Files or directories (default: the current directory).
        paths: Vec<PathBuf>,
        /// Change nothing: list the files that aren't formatted, and fail if there are any.
        #[arg(long)]
        check: bool,
    },
    /// Lint `.pi` files (spec §14.7; rules from `project.json`'s `lint`): warnings about
    /// naming, unused code, complexity, blank lines and docs. Fails if there are any.
    Lint {
        /// Files or directories (default: the project around the current directory).
        paths: Vec<PathBuf>,
        /// Apply the fixes the warnings come with (unused imports, `return`, blank lines),
        /// rewriting the files, then report what's left.
        #[arg(long)]
        fix: bool,
        /// How many files are linted at once (default: the number of CPUs, but no more than
        /// the available memory holds).
        #[arg(short, long)]
        jobs: Option<usize>,
    },
    /// Run the tests in `*.test.pi` files (spec §14.3), each file in a process of its own,
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
        /// Run again whenever a `.pi` file under the paths changes.
        #[arg(long)]
        watch: bool,
        /// How many files run at once (default: the number of CPUs).
        #[arg(short, long)]
        jobs: Option<usize>,
        /// Store the snapshots `toMatchSnapshot` takes again, replacing the old ones (#70).
        #[arg(short = 'u', long)]
        update_snapshots: bool,
    },
    /// Run the benchmarks (`bench(…)` from `std/test`) in `*.bench.pi` and `*.test.pi`
    /// files, one file at a time, optimized.
    Bench {
        /// Files or directories to search (default: the current directory).
        paths: Vec<PathBuf>,
        /// Run in the MIR interpreter instead of native binaries.
        #[arg(long)]
        interp: bool,
    },
    /// Create a project here: `project.json` and `src/main.pi` (an application, with
    /// `"entry": "src/main.pi"`), or `index.pi` with `--lib` (spec §8.3).
    Init {
        /// The package's name, `scope/name` (a library needs one).
        name: Option<String>,
        /// A library: its API is `index.pi`.
        #[arg(long)]
        lib: bool,
    },
    /// Add a git dependency (its name comes from its project.json) and lock it.
    Add {
        /// The repository's URL (or path).
        source: String,
        /// A tag (default: the newest version tag, else the default branch).
        #[arg(long, conflicts_with_all = ["branch", "rev"])]
        tag: Option<String>,
        /// A branch.
        #[arg(long, conflicts_with = "rev")]
        branch: Option<String>,
        /// A commit.
        #[arg(long)]
        rev: Option<String>,
        /// A development dependency (only `*.test.pi` files see it).
        #[arg(long)]
        dev: bool,
    },
    /// Remove a dependency and update the lockfile.
    Remove {
        /// The package's name, `scope/name`.
        name: String,
    },
    /// Generate HTML API docs from doc comments: the project's package (its `index.pi`), or
    /// with `--std` the standard and core libraries.
    Doc {
        /// Document the standard and core libraries.
        #[arg(long)]
        std: bool,
        /// Where to write the pages (default: `target/doc`).
        #[arg(long)]
        out: Option<PathBuf>,
        /// Write the documentation as JSON to this file instead of HTML pages (the website's
        /// reference is made from it).
        #[arg(long, value_name = "FILE", conflicts_with = "out")]
        json: Option<PathBuf>,
    },
    /// Explain an error or lint code: what it means, an example, the fix (`pico explain E0308`).
    Explain {
        /// The code: `E0308`, `308`, `W0001`.
        code: String,
    },
    /// Bring `project.lock` up to date and download every locked package into the cache, so
    /// later builds work offline (spec §8.3).
    Fetch,
    /// Resolve a release build's panic trace (its frames are offsets into the executable) to
    /// pico functions, files and lines, from the debug file the build wrote (spec §7.1).
    Symbolize {
        /// The trace (`-`: standard input).
        trace: PathBuf,
        /// The debug file (default: `<program>.dbg` or `<program>.dSYM` next to the trace).
        #[arg(long)]
        debug: Option<PathBuf>,
    },
    /// The debug adapter (DAP over stdio, on lldb-dap; editors/vscode starts it): builds a
    /// `.pi` program to debug and loads pico's formatters into lldb (spec §14.5).
    Dap,
    /// The language server (LSP over stdio; editors/vscode starts it).
    Lsp {
        /// Accepted for clients that pass it (stdio is the only transport).
        #[arg(long)]
        stdio: bool,
    },
    /// Parse a file and report syntax errors.
    Parse {
        /// The `.pi` file to parse.
        file: PathBuf,
        /// Print the syntax tree as S-expressions.
        #[arg(long)]
        dump: bool,
    },
}

/// The resource directory of pico's own clang (its builtin headers in `include/`): bundled in
/// release builds (`<home>/clang`), else the one of the LLVM pico was built with.
fn clang_resource_dir() -> Option<std::path::PathBuf> {
    pico_runtime::home()
        .map(|h| h.join("clang"))
        .filter(|d| d.join("include/stddef.h").is_file())
        .or_else(|| pico_clang::build_resource_dir().map(Into::into))
}

fn main() -> ExitCode {
    // pico's own clang and archiver (decision 298), run by the driver as `pico clang …` and
    // `pico ar rcsD <out> <members>…`; not subcommands for users.
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("clang") if pico_clang::AVAILABLE => {
            let mut cargs = args[2..].to_vec();
            let cc1 = cargs.first().is_some_and(|a| a.starts_with("-cc1"));
            if !cc1
                && !cargs.iter().any(|a| a.starts_with("-resource-dir"))
                && let Some(rd) = clang_resource_dir()
            {
                cargs.splice(0..0, ["-resource-dir".into(), rd.display().to_string()]);
            }
            std::process::exit(pico_clang::main(&cargs));
        }
        Some("ar") if pico_clang::AVAILABLE && args.len() >= 4 => {
            let members: Vec<&std::path::Path> =
                args[4..].iter().map(std::path::Path::new).collect();
            return match pico_clang::write_archive(std::path::Path::new(&args[3]), &members) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("error: {e}");
                    ExitCode::FAILURE
                }
            };
        }
        _ => {}
    }
    if pico_clang::AVAILABLE
        && let Ok(exe) = std::env::current_exe()
        && let Some(rd) = clang_resource_dir()
    {
        pico_driver::use_own_clang(exe, rd);
    }
    let cli = Cli::parse();
    // Builds may fetch packages and update project.lock; the language server only reads them.
    if !matches!(cli.command, Some(Command::Lsp { .. })) {
        pico_driver::packages::allow_fetching(true);
    }
    match cli.command {
        Some(Command::Parse { file, dump }) => parse(&file, dump),
        Some(Command::Build {
            file,
            output,
            release,
            emit,
            target,
            lib: Some(kind),
            lib_dirs,
            ..
        }) if emit.is_none() => with_entry(file, |file| {
            pico_driver::set_cli_lib_dirs(absolute(&lib_dirs));
            build_lib(
                &file,
                output,
                pico_driver::BuildOptions {
                    release,
                    target,
                    ..Default::default()
                },
                kind,
            )
        }),
        Some(Command::Build {
            file,
            output,
            release,
            emit,
            target,
            force,
            lib_dirs,
            ..
        }) => with_entry(file, |file| {
            pico_driver::set_cli_lib_dirs(absolute(&lib_dirs));
            build(
                &file,
                output,
                pico_driver::BuildOptions {
                    release,
                    target,
                    notices: true,
                },
                emit.as_deref(),
                force,
            )
        }),
        Some(Command::Fmt { paths, check }) => fmt(&paths, check),
        Some(Command::Lint { paths, fix, jobs }) => lint(
            &paths,
            fix,
            jobs.unwrap_or_else(|| default_jobs(LINT_JOB_MB)),
        ),
        Some(Command::Dap) => dap::run(),
        Some(Command::Fetch) => fetch(),
        Some(Command::Doc { std, out, json }) => doc::run(std, out.as_deref(), json.as_deref()),
        Some(Command::Explain { code }) => {
            match pico_diag::codes::parse(&code)
                .and_then(|c| Some((c, pico_diag::codes::explain(c)?)))
            {
                Some((_, text)) => {
                    print!("{text}");
                    ExitCode::SUCCESS
                }
                None => match pico_diag::codes::parse(&code) {
                    Some(c) => {
                        println!(
                            "{c}: {}

(no longer explanation yet)",
                            pico_diag::codes::describe(c).unwrap_or_default()
                        );
                        ExitCode::SUCCESS
                    }
                    None => {
                        eprintln!(
                            "error: `{code}` isn't an error or lint code (they look like E0308, W0001)"
                        );
                        ExitCode::from(2)
                    }
                },
            }
        }
        Some(Command::Init { name, lib }) => {
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            match pico_driver::packages::init(&cwd, name.as_deref(), lib) {
                Ok(files) => {
                    for f in files {
                        println!("created {}", f.strip_prefix(&cwd).unwrap_or(&f).display());
                    }
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        Some(Command::Add {
            source,
            tag,
            branch,
            rev,
            dev,
        }) => {
            use pico_driver::packages::GitPin;
            let pin = match (tag, branch, rev) {
                (Some(t), _, _) => GitPin::Tag(t),
                (_, Some(b), _) => GitPin::Branch(b),
                (_, _, Some(r)) => GitPin::Rev(r),
                _ => GitPin::Default,
            };
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            match pico_driver::packages::add_git(&cwd, &source, pin, dev) {
                Ok((name, pinned)) => {
                    println!("added {name} ({source}, {pinned})");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        Some(Command::Remove { name }) => {
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            match pico_driver::packages::remove(&cwd, &name) {
                Ok(()) => {
                    println!("removed {name}");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        Some(Command::Symbolize { trace, debug }) => symbolize::run(&trace, debug.as_deref()),
        Some(Command::Lsp { .. }) => match pico_lsp::run_stdio() {
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
            lib_dirs,
            target,
            args,
        }) => with_entry(file, |file| {
            pico_driver::set_cli_lib_dirs(absolute(&lib_dirs));
            if interp && target.is_some() {
                eprintln!("error: `--interp` runs the program here; it takes no `--target`");
                return ExitCode::from(2);
            }
            run(
                &file,
                interp,
                pico_driver::BuildOptions {
                    release,
                    target,
                    ..Default::default()
                },
                &args,
            )
        }),
        Some(Command::Test {
            paths,
            filter,
            interp,
            release,
            watch,
            jobs,
            update_snapshots,
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
                jobs: jobs.unwrap_or_else(|| default_jobs(TEST_JOB_MB)),
                update_snapshots,
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
            update_snapshots: false,
        }
        .run(),
        None => ExitCode::SUCCESS,
    }
}

/// `pico test` / `pico bench`: finds the files and runs each through `pico run` in a child
/// process (isolated: a crash or `exit` ends only that file), collecting their reports.
struct TestRun {
    paths: Vec<PathBuf>,
    filter: Option<String>,
    interp: bool,
    release: bool,
    bench: bool,
    jobs: usize,
    /// `-u`: `toMatchSnapshot` stores its snapshots again.
    update_snapshots: bool,
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
        name.ends_with(".test.pi") || (self.bench && name.ends_with(".bench.pi"))
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
        let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("pico"));
        let mut cmd = std::process::Command::new(exe);
        cmd.arg("run");
        if self.interp {
            cmd.arg("--interp");
        }
        if self.release {
            cmd.arg("--release");
        }
        cmd.arg(file);
        cmd.env("PICO_TEST_MODE", if self.bench { "bench" } else { "test" });
        if self.update_snapshots {
            cmd.env("PICO_TEST_UPDATE", "1");
        }
        match &self.filter {
            Some(f) => cmd.env("PICO_TEST_FILTER", f),
            None => cmd.env_remove("PICO_TEST_FILTER"),
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
            if let Some(rest) = line.strip_prefix("pico-test: ") {
                // "N passed, M failed, K skipped"
                let n: Vec<u64> = rest
                    .split(", ")
                    .filter_map(|p| p.split(' ').next()?.parse().ok())
                    .collect();
                if let [a, b, c] = n[..] {
                    counts = Some((a, b, c));
                }
            } else if let Some(rest) = line.strip_prefix("pico-bench: ") {
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
                    "*.bench.pi or *.test.pi"
                } else {
                    "*.test.pi"
                }
            );
            return ExitCode::FAILURE;
        }
        self.run_files(&files)
    }

    fn run_files(&self, files: &[PathBuf]) -> ExitCode {
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

    /// Runs, then again after each change to a `.pi` file under the paths (polled): only the
    /// test files that the change affects, those that are, or import (directly or not), a
    /// changed, added or removed file.
    fn watch(&self) -> ExitCode {
        type Stamps = std::collections::BTreeMap<PathBuf, std::time::SystemTime>;
        fn stamp(paths: &[PathBuf]) -> Stamps {
            fn walk(dir: &Path, out: &mut Stamps) {
                let Ok(rd) = std::fs::read_dir(dir) else {
                    return;
                };
                for e in rd.flatten() {
                    let p = e.path();
                    let name = e.file_name().to_string_lossy().into_owned();
                    if p.is_dir() {
                        if !name.starts_with('.') && name != "target" && name != "node_modules" {
                            walk(&p, out);
                        }
                    } else if name.ends_with(".pi")
                        && let Ok(t) = e.metadata().and_then(|m| m.modified())
                    {
                        out.insert(canonical(&p), t);
                    }
                }
            }
            let mut out = Stamps::new();
            for p in paths {
                if p.is_dir() {
                    walk(p, &mut out);
                } else if let Ok(t) = std::fs::metadata(p).and_then(|m| m.modified()) {
                    out.insert(canonical(p), t);
                }
            }
            out
        }
        fn canonical(p: &Path) -> PathBuf {
            pico_runtime::canonical(p).unwrap_or_else(|_| p.to_path_buf())
        }
        let mut before = stamp(&self.paths);
        self.run();
        loop {
            println!("\nwatching for changes (ctrl-c to stop)…");
            let mut now = stamp(&self.paths);
            while now == before {
                std::thread::sleep(std::time::Duration::from_millis(300));
                now = stamp(&self.paths);
            }
            let changed: std::collections::BTreeSet<&PathBuf> = before
                .iter()
                .filter(|(p, t)| now.get(*p) != Some(t))
                .map(|(p, _)| p)
                .chain(now.keys().filter(|p| !before.contains_key(*p)))
                .collect();
            let affected: Vec<PathBuf> = self
                .files()
                .into_iter()
                .filter(|f| {
                    // Unknown imports (one can't be found): any change may matter.
                    pico_driver::import_files(f)
                        .is_none_or(|deps| deps.iter().any(|d| changed.contains(d)))
                })
                .collect();
            println!();
            if affected.is_empty() {
                println!("no test file depends on the change");
            } else {
                self.run_files(&affected);
            }
            before = now;
        }
    }
}

fn load(path: &Path, opts: pico_driver::BuildOptions) -> Option<pico_driver::Checked> {
    // `pico_driver::check_file` names the file by its path, which then appears in diagnostics.
    match pico_driver::check_file_with(path, opts) {
        Ok(c) => Some(c),
        Err(e) => {
            eprintln!("error: cannot read {}: {e}", path.display());
            None
        }
    }
}

fn checked_or_report(
    path: &Path,
    opts: pico_driver::BuildOptions,
) -> Result<pico_driver::Checked, ExitCode> {
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
        explain_hint(&checked.diags);
        return Err(ExitCode::FAILURE);
    }
    Ok(checked)
}

/// After errors: which `pico explain` explains them.
fn explain_hint(diags: &[pico_diag::Diagnostic]) {
    let mut codes: Vec<String> = diags
        .iter()
        .filter(|d| d.is_error())
        .filter_map(|d| d.code)
        .filter(|c| pico_diag::codes::explain(*c).is_some())
        .map(|c| c.to_string())
        .collect();
    codes.sort();
    codes.dedup();
    match codes.as_slice() {
        [] => {}
        [one] => eprintln!("For more about this error, try `pico explain {one}`."),
        many => eprintln!(
            "Some errors are explained: {}; try `pico explain {}`.",
            many.join(", "),
            many[0]
        ),
    }
}

fn run(path: &Path, interp: bool, opts: pico_driver::BuildOptions, args: &[String]) -> ExitCode {
    let checked = match checked_or_report(path, opts) {
        Ok(c) => c,
        Err(code) => return code,
    };
    if interp {
        let mir = checked.mir.as_ref().expect("MIR after a clean check");
        let mut out = std::io::stdout();
        return match pico_driver::run_interp_with(
            mir,
            &mut out,
            None,
            args.to_vec(),
            Some(path.to_path_buf()),
        ) {
            Ok(()) => ExitCode::SUCCESS,
            Err(p) if p.exit.is_some() => ExitCode::from(p.exit.unwrap_or(0).clamp(0, 255) as u8),
            Err(p) => {
                eprint!("{}", p.report());
                ExitCode::from(if p.uncaught {
                    pico_driver::UNCAUGHT_EXIT_CODE
                } else {
                    pico_driver::PANIC_EXIT_CODE
                } as u8)
            }
        };
    }
    let exe = match pico_driver::build_cached(&checked, opts) {
        Ok(exe) => exe,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };
    timings_peak();
    let mut cmd = match run_command(&exe, opts.target) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };
    match pico_driver::spawn::status(cmd.args(args)) {
        Ok(status) => ExitCode::from(status.code().unwrap_or(1).clamp(0, 255) as u8),
        Err(e) => {
            eprintln!("error: cannot run {}: {e}", exe.display());
            ExitCode::FAILURE
        }
    }
}

/// How to run `exe`, built for `target` (#77): directly for the host, under wasmtime for
/// `wasm32-wasi` (as the golden tests run it), under Node.js for `wasm32-browser`, and for
/// another Linux architecture directly (binfmt's qemu-user) with `QEMU_LD_PREFIX` set when the
/// system has that architecture's libraries. macOS and Windows programs only run on their OS.
fn run_command(
    exe: &Path,
    target: Option<pico_driver::Target>,
) -> Result<std::process::Command, String> {
    use pico_driver::target::Os;
    let Some(t) = target.filter(|t| !t.is_host()) else {
        return Ok(std::process::Command::new(exe));
    };
    match t.os {
        Os::Wasi => {
            let mut c = std::process::Command::new("wasmtime");
            c.args([
                "run",
                "-W",
                "exceptions=y",
                "--dir=.",
                "-S",
                "inherit-network=y",
                "-S",
                "allow-ip-name-lookup=y",
            ]);
            c.arg(exe);
            Ok(c)
        }
        Os::Browser => {
            let mut c = std::process::Command::new("node");
            c.arg(exe);
            Ok(c)
        }
        Os::Linux if cfg!(target_os = "linux") => {
            let mut c = std::process::Command::new(exe);
            let prefix = PathBuf::from(format!("/usr/{}", t.multiarch()));
            if prefix.is_dir() {
                c.env("QEMU_LD_PREFIX", prefix);
            }
            Ok(c)
        }
        _ => Err(format!(
            "a {t} program can't run on this machine; build it with `pico build --target {t}` and run it there"
        )),
    }
}

/// With `PICO_TIMINGS=1`: the compiler's peak memory once a build is done (code generation and
/// linking included; the program's own memory isn't counted), #134.
fn timings_peak() {
    if std::env::var_os("PICO_TIMINGS").is_some_and(|v| v == "1")
        && let Some(mb) = pico_driver::peak_memory_mb()
    {
        eprintln!("[timings] built: peak {mb} MB");
    }
}

/// `-L` directories, absolute (a build may run elsewhere than the current directory).
fn absolute(dirs: &[PathBuf]) -> Vec<PathBuf> {
    dirs.iter()
        .map(|d| std::path::absolute(d).unwrap_or_else(|_| d.clone()))
        .collect()
}

/// `pico fmt [--check] [paths…]`.
fn fmt(paths: &[PathBuf], check: bool) -> ExitCode {
    let roots: Vec<PathBuf> = if paths.is_empty() {
        vec![PathBuf::from(".")]
    } else {
        paths.to_vec()
    };
    let mut files = Vec::new();
    for r in &roots {
        pico_files(r, &mut files);
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
        let (_, diags) = pico_syntax::parse_module(pico_diag::FileId(0), &text);
        if diags.iter().any(|d| d.is_error()) {
            eprintln!("skipping {}: it has syntax errors", f.display());
            continue;
        }
        let dir = f.parent().unwrap_or(Path::new("."));
        let opts = pico_driver::fmt_options(dir);
        match pico_syntax::fmt::format(&text, opts) {
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
            eprintln!("{unformatted} file(s) aren't formatted (run `pico fmt`)");
        }
        return ExitCode::FAILURE;
    }
    if !check {
        println!("formatted {unformatted} of {} file(s)", files.len());
    }
    ExitCode::SUCCESS
}

fn fetch() -> ExitCode {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    match pico_driver::packages::prepare_with(&cwd, true) {
        Ok(Some(p)) => {
            let git = p
                .lock
                .packages
                .values()
                .filter(|l| matches!(l.source, pico_driver::packages::LockSource::Git { .. }))
                .count();
            println!(
                "{} package(s) locked in {}, {git} from git, all in the cache",
                p.lock.packages.len(),
                p.root.join("project.lock").display()
            );
            ExitCode::SUCCESS
        }
        Ok(None) => {
            eprintln!("error: no project.json here or above");
            ExitCode::from(2)
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Runs `f` on the file given, or else on the project's `entry` (spec §8.3).
fn with_entry(file: Option<PathBuf>, f: impl FnOnce(PathBuf) -> ExitCode) -> ExitCode {
    match file {
        Some(file) => f(file),
        None => {
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            match pico_driver::packages::entry_of(&cwd) {
                Ok(entry) => f(entry),
                Err(e) => {
                    eprintln!("error: {e}");
                    ExitCode::from(2)
                }
            }
        }
    }
}

/// About what one `pico lint` worker needs at most (a file with everything it imports, checked;
/// about 200 MB for an 80-file application, #166).
const LINT_JOB_MB: u64 = 256;
/// About what one test file's process needs while it's compiled and run.
const TEST_JOB_MB: u64 = 512;

/// How many jobs run at once by default: one per CPU, but no more than the available memory
/// holds at `per_job_mb` each (on Linux, `MemAvailable`; elsewhere the CPUs), at least one.
fn default_jobs(per_job_mb: u64) -> usize {
    let cpus = std::thread::available_parallelism().map_or(4, std::num::NonZero::get);
    match available_memory_mb() {
        Some(mb) => cpus.min(((mb / per_job_mb) as usize).max(1)),
        None => cpus,
    }
}

/// The memory available to new work, in MB (Linux: `/proc/meminfo`'s `MemAvailable`).
fn available_memory_mb() -> Option<u64> {
    let info = std::fs::read_to_string("/proc/meminfo").ok()?;
    let line = info.lines().find(|l| l.starts_with("MemAvailable:"))?;
    let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb / 1024)
}

fn lint(paths: &[PathBuf], fix: bool, jobs: usize) -> ExitCode {
    let mut files = Vec::new();
    if paths.is_empty() {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        files = pico_driver::project_files(&pico_driver::project_root(&cwd.join("_")));
    } else {
        for p in paths {
            pico_files(p, &mut files);
        }
    }
    files.sort();
    // Each file is checked on its own (with what it imports), so the files are linted in
    // parallel; each worker renders its file's report, printed afterwards in file order.
    let next = std::sync::atomic::AtomicUsize::new(0);
    let reports: std::sync::Mutex<Vec<(usize, LintReport)>> = std::sync::Mutex::new(Vec::new());
    std::thread::scope(|s| {
        for _ in 0..jobs.max(1).min(files.len()) {
            std::thread::Builder::new()
                .name("pico-lint".into())
                // (Decorators and comptime code run in the interpreter, which recurses deeply.)
                .stack_size(1 << 28)
                .spawn_scoped(s, || {
                    loop {
                        let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let Some(f) = files.get(i) else { break };
                        let r = lint_one(f, fix);
                        reports.lock().expect("lint reports").push((i, r));
                    }
                })
                .expect("spawn lint thread");
        }
    });
    let mut reports = reports.into_inner().expect("lint reports");
    reports.sort_by_key(|(i, _)| *i);
    let (mut warnings, mut errors, mut config_errors) = (0, 0, std::collections::BTreeSet::new());
    for (_, r) in reports {
        warnings += r.warnings;
        errors += r.errors;
        config_errors.extend(r.config_errors);
        eprint!("{}", r.output);
    }
    for e in &config_errors {
        eprintln!("error: {e}");
    }
    if errors + warnings + config_errors.len() > 0 {
        eprintln!(
            "{warnings} warning(s), {} error(s) in {} file(s)",
            errors + config_errors.len(),
            files.len()
        );
        return ExitCode::FAILURE;
    }
    println!("no warnings in {} file(s)", files.len());
    ExitCode::SUCCESS
}

/// What linting one file found: its rendered diagnostics and their counts.
struct LintReport {
    warnings: usize,
    errors: usize,
    config_errors: Vec<String>,
    output: String,
}

fn lint_one(f: &Path, fix: bool) -> LintReport {
    let Ok(mut text) = std::fs::read_to_string(f) else {
        return LintReport {
            warnings: 0,
            errors: 1,
            config_errors: Vec::new(),
            output: format!("error: cannot read {}\n", f.display()),
        };
    };
    // (`pico lint main.pi`: its parent is "", the current directory.)
    let dir = f
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let (cfg, config_errors) = pico_lint::Config::for_dir(dir);
    let (mut checked, mut found) = pico_lint::lint_file(f, &text, &cfg);
    // `--fix` (#80): apply the warnings' fixes and lint again, until none are left.
    let mut fixed = false;
    for _ in 0..10 {
        if !fix {
            break;
        }
        let entry = pico_lint::entry_file(&checked);
        let Some(new) = pico_lint::apply_fixes(&text, entry, &found) else {
            break;
        };
        if std::fs::write(f, &new).is_err() {
            break;
        }
        text = new;
        fixed = true;
        (checked, found) = pico_lint::lint_file(f, &text, &cfg);
    }
    // (Only the file's own errors: an imported module's are reported with it.)
    let entry = pico_lint::entry_file(&checked);
    let own: Vec<pico_diag::Diagnostic> = checked
        .diags
        .iter()
        .filter(|d| d.is_error() && d.main_span().is_none_or(|s| s.file == entry))
        .cloned()
        .collect();
    let mut output = if fixed {
        format!("fixed {}\n", f.display())
    } else {
        String::new()
    };
    output.push_str(&pico_diag::render_all(&own, &checked.sm));
    output.push_str(&pico_diag::render_all(&found, &checked.sm));
    LintReport {
        warnings: found.len(),
        errors: own.len(),
        config_errors,
        output,
    }
}

/// The `.pi` files at `p` (a file, or a directory searched recursively, skipping hidden
/// directories, `target` and `node_modules`).
fn pico_files(p: &Path, out: &mut Vec<PathBuf>) {
    if p.is_file() {
        if p.extension().is_some_and(|e| e == "pi") {
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
        pico_files(&path, out);
    }
}

fn parse_lib(s: &str) -> Result<pico_driver::LibKind, String> {
    match s {
        "static" => Ok(pico_driver::LibKind::Static),
        "shared" => Ok(pico_driver::LibKind::Shared),
        _ => Err(format!("`{s}`: expected `static` or `shared`")),
    }
}

/// `pico build --lib static|shared`: `lib<name>.a`/`.so` and `lib<name>.h` by default.
fn build_lib(
    path: &Path,
    output: Option<PathBuf>,
    opts: pico_driver::BuildOptions,
    kind: pico_driver::LibKind,
) -> ExitCode {
    let checked = match checked_or_report(path, opts) {
        Ok(c) => c,
        Err(code) => return code,
    };
    let out = output.unwrap_or_else(|| {
        let stem = path.file_stem().unwrap_or_default().to_string_lossy();
        let ext = match kind {
            pico_driver::LibKind::Static => "a",
            pico_driver::LibKind::Shared => "so",
        };
        PathBuf::from(format!("lib{stem}.{ext}"))
    });
    match pico_driver::build_library(&checked, &out, opts, kind) {
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
    opts: pico_driver::BuildOptions,
    emit: Option<&str>,
    force: bool,
) -> ExitCode {
    let exe = output
        .clone()
        .unwrap_or_else(|| PathBuf::from(path.file_stem().unwrap_or_default()));
    // Nothing changed since the last build: done (spec §14.6).
    if emit.is_none()
        && !force
        && pico_driver::fingerprint::up_to_date(path, &exe, &opts)
        && (!opts.notices || pico_driver::notices_file(&exe).exists())
    {
        eprintln!("{} is up to date", exe.display());
        return ExitCode::SUCCESS;
    }
    let checked = match checked_or_report(path, opts) {
        Ok(c) => c,
        Err(code) => return code,
    };
    let mir = checked.mir.as_ref().expect("MIR after a clean check");
    match emit {
        Some("mir") => {
            print!("{}", pico_mir::dump(mir));
            ExitCode::SUCCESS
        }
        // (Without `--target`, the IR is the target-independent form the snapshot tests use.)
        Some("llvm-ir") => match opts.target {
            None => {
                print!("{}", pico_codegen::emit_llvm_ir(mir));
                ExitCode::SUCCESS
            }
            Some(t) => {
                let o = pico_codegen::Options {
                    optimize: opts.release,
                    debug: false,
                    triple: Some(t.llvm_triple()),
                };
                match pico_codegen::emit_llvm_ir_for(mir, o) {
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
        None => match pico_driver::build_native(&checked, &exe, opts) {
            Ok(()) => {
                pico_driver::fingerprint::record(&checked, path, &exe, &opts);
                timings_peak();
                ExitCode::SUCCESS
            }
            Err(e) => {
                pico_driver::fingerprint::forget(&exe);
                eprintln!("error: {e}");
                ExitCode::FAILURE
            }
        },
    }
}

fn parse(path: &Path, dump: bool) -> ExitCode {
    let checked = match pico_driver::check_file(path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: cannot read {}: {e}", path.display());
            return ExitCode::from(2);
        }
    };
    if dump {
        print!("{}", pico_syntax::dump::dump_module(&checked.module));
    }
    if checked.diags.is_empty() {
        ExitCode::SUCCESS
    } else {
        eprint!("{}", checked.render_diagnostics());
        ExitCode::FAILURE
    }
}
