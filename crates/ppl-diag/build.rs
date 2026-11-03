//! Embeds the long-form explanations of the error and lint codes (`docs/errors/E0308.md`, …)
//! for `ppl explain` (spec §14.2).

use std::io::Write;
use std::path::PathBuf;

fn main() {
    let dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("../../docs/errors");
    println!("cargo:rerun-if-changed={}", dir.display());
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .map(|r| r.filter_map(|e| Some(e.ok()?.path())).collect())
        .unwrap_or_default();
    files.retain(|p| p.extension().is_some_and(|e| e == "md"));
    files.sort();
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("explanations.rs");
    let mut f = std::fs::File::create(out).unwrap();
    writeln!(
        f,
        "/// Each code's explanation (Markdown), by code (`E0308`, `W0001`)."
    )
    .unwrap();
    writeln!(f, "pub static EXPLANATIONS: &[(&str, &str)] = &[").unwrap();
    for p in files {
        println!("cargo:rerun-if-changed={}", p.display());
        let code = p.file_stem().unwrap().to_string_lossy().to_string();
        let path = p.canonicalize().unwrap();
        writeln!(
            f,
            "    ({code:?}, include_str!({:?})),",
            path.display().to_string()
        )
        .unwrap();
    }
    writeln!(f, "];").unwrap();
}
