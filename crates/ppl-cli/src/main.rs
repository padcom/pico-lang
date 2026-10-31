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
        None => ExitCode::SUCCESS,
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
