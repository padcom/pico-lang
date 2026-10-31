//! The `ppl` command-line tool.

use clap::{Parser, Subcommand};
use ppl_diag::{SourceMap, render_all};
use std::path::PathBuf;
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

fn parse(path: &PathBuf, dump: bool) -> ExitCode {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("error: cannot read {}: {e}", path.display());
            return ExitCode::from(2);
        }
    };
    let mut sm = SourceMap::new();
    let file = sm.add(path.display().to_string(), text);
    let (module, diags) = ppl_syntax::parse_module(file, &sm.file(file).text);
    if dump {
        print!("{}", ppl_syntax::dump::dump_module(&module));
    }
    if diags.is_empty() {
        ExitCode::SUCCESS
    } else {
        eprint!("{}", render_all(&diags, &sm));
        ExitCode::FAILURE
    }
}
