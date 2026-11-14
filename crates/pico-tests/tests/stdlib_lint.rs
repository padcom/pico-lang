//! The standard library follows the lint rules that are on by default (spec §14.7): every
//! `stdlib/**/*.pi` file, linted with the default configuration, has no warnings.

use std::path::{Path, PathBuf};

fn files(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    entries.sort();
    for p in entries {
        if p.is_dir() {
            files(&p, out);
        } else if p.extension().is_some_and(|e| e == "pi") {
            out.push(p);
        }
    }
}

/// Names that the Web platform or the spec fixes, so the rules can't have their way:
/// (file, name, why).
const EXCEPTIONS: &[(&str, &str, &str)] = &[
    ("core/time.pi", "`UTC`", "JavaScript's `Date.UTC`"),
    (
        "core/worker.pi",
        "`navigator`",
        "the Web's `navigator` global",
    ),
    ("gc.pi", "`gc`", "std/gc's `gc` (decision 206)"),
    (
        "core/crypto.pi",
        "`crypto`",
        "a core-library global, used by programs",
    ),
    (
        "core/time.pi",
        "`performance`",
        "a core-library global, used by programs",
    ),
];

/// Files that don't check on their own, as a program's root (file, why).
const CHECK_ERRORS: &[(&str, &str)] = &[
    (
        "core/weak.pi",
        "uses decorators, which aren't implemented on their own yet (E0900)",
    ),
    (
        "core/string.pi",
        "declares the runtime's own C functions with `string` (E0355)",
    ),
];

#[test]
fn stdlib_follows_the_default_lint_rules() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../stdlib");
    let mut all = Vec::new();
    files(&root, &mut all);
    assert!(
        all.len() > 10,
        "no stdlib files found in {}",
        root.display()
    );
    let cfg = pico_lint::Config::default();
    let mut problems = Vec::new();
    for f in &all {
        let text = std::fs::read_to_string(f).unwrap();
        let (checked, warnings) = pico_lint::lint_file(f, &text, &cfg);
        let rel = f.strip_prefix(&root).unwrap_or(f).display().to_string();
        // A file that doesn't check can't be linted fully (`unused` needs its types).
        for e in checked.diags.iter().filter(|d| d.is_error()) {
            if !CHECK_ERRORS.iter().any(|(file, _)| rel == *file) {
                problems.push(format!("{rel}: doesn't check on its own: {}", e.message));
            }
        }
        for w in &warnings {
            if EXCEPTIONS
                .iter()
                .any(|(file, name, _)| rel == *file && w.message.contains(name))
            {
                continue;
            }
            let at = w.main_span().map(|s| {
                let (l, c) = checked.sm.file(s.file).line_col(s.start);
                format!("{l}:{c}")
            });
            problems.push(format!(
                "{}:{} {} {}",
                f.strip_prefix(&root).unwrap_or(f).display(),
                at.unwrap_or_default(),
                w.code.map(|c| c.to_string()).unwrap_or_default(),
                w.message
            ));
        }
    }
    assert!(
        problems.is_empty(),
        "{} lint warning(s) in the standard library:\n{}",
        problems.len(),
        problems.join("\n")
    );
}
