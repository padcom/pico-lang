//! Every ```ts code block in the spec (and the tour) must parse without syntax errors.
//!
//! The spec uses `…` as a placeholder for omitted code; it's replaced before parsing (`{ … }`
//! becomes `{}`, other `…` are removed). Blocks that aren't programs (token catalogues) or that
//! show syntax errors on purpose are listed in `SKIP` with the reason.

use pico_diag::{SourceMap, render_all};
use std::path::{Path, PathBuf};

/// (file, 1-based index of the ```ts block in that file, reason)
const SKIP: &[(&str, usize, &str)] = &[
    (
        "01-lexical.md",
        2,
        "a catalogue of literal forms, one per column, not a program",
    ),
    (
        "07-errors.md",
        4,
        "a method signature quoted out of its class body",
    ),
];

/// Tour blocks that intentionally contain syntax errors or syntax the spec doesn't define yet.
const TOUR_SKIP: &[(usize, &str)] = &[(39, "shows a positional argument after a named one")];

struct Block {
    file: String,
    index: usize,
    line: usize,
    code: String,
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Extracts ```ts blocks (with 3 or more backticks in the fence).
fn ts_blocks(path: &Path) -> Vec<Block> {
    let text = std::fs::read_to_string(path).unwrap();
    let file = path.file_name().unwrap().to_string_lossy().into_owned();
    let mut out = Vec::new();
    let mut lines = text.lines().enumerate();
    let mut index = 0;
    while let Some((n, line)) = lines.next() {
        let trimmed = line.trim_start();
        let ticks = trimmed.chars().take_while(|&c| c == '`').count();
        if ticks < 3 || trimmed[ticks..].trim() != "ts" {
            continue;
        }
        let indent = line.len() - trimmed.len();
        let fence = "`".repeat(ticks);
        let mut code = String::new();
        for (_, l) in lines.by_ref() {
            if l.trim() == fence {
                break;
            }
            code.push_str(l.get(indent..).unwrap_or(l.trim_start()));
            code.push('\n');
        }
        index += 1;
        out.push(Block {
            file: file.clone(),
            index,
            line: n + 1,
            code,
        });
    }
    out
}

fn fill_placeholders(code: &str) -> String {
    code.replace("{ … }", "{}")
        .lines()
        .filter(|l| l.trim() != "…")
        .collect::<Vec<_>>()
        .join("\n")
        .replace("…", "")
}

fn check(blocks: Vec<Block>, skip: impl Fn(&Block) -> bool) -> Vec<String> {
    let mut failures = Vec::new();
    for b in blocks {
        if skip(&b) {
            continue;
        }
        let code = fill_placeholders(&b.code);
        let mut sm = SourceMap::new();
        let name = format!("{}#{} (line {})", b.file, b.index, b.line);
        let file = sm.add(name.clone(), code.clone());
        let (_, diags) = pico_syntax::parse_module(file, &code);
        if !diags.is_empty() {
            failures.push(format!("── {name}\n{}", render_all(&diags, &sm)));
        }
    }
    failures
}

#[test]
fn spec_samples_parse() {
    let dir = repo_root().join("docs/spec");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "md"))
        .collect();
    files.sort();
    let mut failures = Vec::new();
    let mut count = 0;
    for f in files {
        let blocks = ts_blocks(&f);
        count += blocks.len();
        failures.extend(check(blocks, |b| {
            SKIP.iter()
                .any(|(file, idx, _)| *file == b.file && *idx == b.index)
        }));
    }
    assert!(count > 50, "found only {count} spec samples");
    assert!(
        failures.is_empty(),
        "{} spec samples failed to parse:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn tour_samples_parse() {
    let blocks = ts_blocks(&repo_root().join("docs/TOUR.md"));
    let failures = check(blocks, |b| TOUR_SKIP.iter().any(|(idx, _)| *idx == b.index));
    assert!(
        failures.is_empty(),
        "{} tour samples failed to parse:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
