//! The `ppl` command-line tool.

use clap::Parser;

/// The padcom programming language toolchain.
#[derive(Parser)]
#[command(name = "ppl", version, about)]
struct Cli {}

fn main() {
    let _cli = Cli::parse();
}
