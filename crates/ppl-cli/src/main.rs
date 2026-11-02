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
        }) => build(
            &file,
            output,
            ppl_driver::BuildOptions { release },
            emit.as_deref(),
        ),
        Some(Command::Run {
            file,
            interp,
            release,
            args,
        }) => run(&file, interp, ppl_driver::BuildOptions { release }, &args),
        None => ExitCode::SUCCESS,
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
        Some("llvm-ir") => {
            print!("{}", ppl_codegen::emit_llvm_ir(mir));
            ExitCode::SUCCESS
        }
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
