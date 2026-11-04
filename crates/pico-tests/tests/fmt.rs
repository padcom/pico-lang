//! `pico fmt` (spec §14.4, decision 265): every `.pi` file of the repository that parses (the
//! golden fixtures, the standard library, the examples) is formatted, and formatting is
//! idempotent. Fix with `pico fmt tests stdlib examples`.

use std::path::{Path, PathBuf};

fn files(p: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(p) else {
        return;
    };
    for e in entries.flatten() {
        let path = e.path();
        if path.is_dir() {
            if e.file_name() != "node_modules" {
                files(&path, out);
            }
        } else if path.extension().is_some_and(|x| x == "pi") {
            out.push(path);
        }
    }
}

#[test]
fn sources_are_formatted() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut all = Vec::new();
    for d in ["tests", "stdlib", "examples"] {
        files(&root.join(d), &mut all);
    }
    all.sort();
    assert!(all.len() > 100, "found only {} files", all.len());
    let opts = pico_syntax::fmt::FmtOptions::default();
    let mut bad = Vec::new();
    for f in &all {
        let text = std::fs::read_to_string(f).unwrap();
        let (_, diags) = pico_syntax::parse_module(pico_diag::FileId(0), &text);
        if diags.iter().any(|d| d.is_error()) {
            continue; // (fixtures with syntax errors on purpose)
        }
        let once = pico_syntax::fmt::format(&text, opts).unwrap();
        let twice = pico_syntax::fmt::format(&once, opts).unwrap();
        if once != twice {
            bad.push(format!("{}: formatting isn't idempotent", f.display()));
        } else if once != text {
            bad.push(format!("{}: not formatted", f.display()));
        }
    }
    assert!(
        bad.is_empty(),
        "\n{}\n(run `pico fmt tests stdlib examples`)",
        bad.join("\n")
    );
}
