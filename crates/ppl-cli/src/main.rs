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
    /// Compile a program.
    Build {
        /// The entry `.ppl` file.
        file: PathBuf,
        /// Print an intermediate form instead of building: `mir`.
        #[arg(long, value_name = "FORM")]
        emit: Option<String>,
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
        Some(Command::Build { file, emit }) => build(&file, emit.as_deref()),
        None => ExitCode::SUCCESS,
    }
}

fn load(path: &Path) -> Option<ppl_driver::Checked> {
    match ppl_driver::check_file(path) {
        Ok(c) => Some(c),
        Err(e) => {
            eprintln!("error: cannot read {}: {e}", path.display());
            None
        }
    }
}

fn build(path: &Path, emit: Option<&str>) -> ExitCode {
    let Some(checked) = load(path) else {
        return ExitCode::from(2);
    };
    if checked.has_errors() {
        eprint!("{}", checked.render_diagnostics());
        return ExitCode::FAILURE;
    }
    match emit {
        Some("mir") => {
            print!(
                "{}",
                ppl_mir::dump(checked.mir.as_ref().expect("MIR after a clean check"))
            );
            ExitCode::SUCCESS
        }
        Some(other) => {
            eprintln!("error: unknown --emit form `{other}` (expected `mir`)");
            ExitCode::from(2)
        }
        None => {
            eprintln!("error: native builds aren't implemented yet; use --emit mir");
            ExitCode::from(2)
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
