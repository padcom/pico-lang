//! `pico explain` (spec §14.2, PLAN P10.9a): every error and lint code has an explanation in
//! `docs/errors/<code>.md`, and its examples are real: the first ```` ```ts ```` block (docs' fence for pico) reports
//! the code, the second (the fix) doesn't. Blocks marked ```` ```ts ignore ```` (examples
//! that need several files or another target) aren't checked.
//!
//! `EXPLAIN_ONLY=E03` checks only the codes starting so; `EXPLAIN_PARTIAL=1` skips the check
//! that every code has an explanation (while writing them).

use pico_diag::codes;

/// The `pico` blocks of an explanation: (checked, text).
fn blocks(md: &str) -> Vec<(bool, String)> {
    let mut out = Vec::new();
    let mut cur: Option<(bool, String)> = None;
    for line in md.lines() {
        match &mut cur {
            None => {
                if let Some(info) = line.strip_prefix("```ts") {
                    cur = Some((info.trim() != "ignore", String::new()));
                }
            }
            Some((checked, text)) => {
                if line.starts_with("```") {
                    out.push((*checked, std::mem::take(text)));
                    cur = None;
                } else {
                    text.push_str(line);
                    text.push('\n');
                }
            }
        }
    }
    out
}

/// The codes `text` reports: the compiler's, and the lint's with every rule on.
fn reported(text: &str, dir: &std::path::Path) -> Vec<String> {
    let file = dir.join("example.pi");
    std::fs::write(&file, text).unwrap();
    let checked = pico_driver::check_source_with(
        file.display().to_string(),
        text.to_string(),
        dir,
        pico_driver::BuildOptions::default(),
    );
    let mut codes: Vec<String> = checked
        .diags
        .iter()
        .filter(|d| d.is_error())
        .filter_map(|d| d.code.map(|c| c.to_string()))
        .collect();
    let cfg = pico_lint::Config {
        naming: true,
        unused: true,
        complexity: Some(4),
        blank_lines: Some(
            pico_lint::config::CONSTRUCTS
                .iter()
                .map(|s| s.to_string())
                .collect(),
        ),
        docs: true,
    };
    codes.extend(
        pico_lint::lint(&checked, pico_lint::entry_file(&checked), &cfg)
            .iter()
            .filter_map(|d| d.code.map(|c| c.to_string())),
    );
    codes
}

#[test]
fn every_code_is_explained_with_real_examples() {
    let only = std::env::var("EXPLAIN_ONLY").unwrap_or_default();
    let partial = std::env::var_os("EXPLAIN_PARTIAL").is_some();
    let dir = std::env::temp_dir().join(format!("pico-explain-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let all: Vec<(String, &str)> = codes::ALL
        .iter()
        .map(|(c, d)| (c.to_string(), *d))
        .chain(codes::LINTS.iter().map(|(c, _, d)| (c.to_string(), *d)))
        .filter(|(c, _)| c.starts_with(&only))
        .collect();
    let mut problems = Vec::new();
    for (code, _) in &all {
        let Some(md) = codes::explain(codes::parse(code).unwrap()) else {
            if !partial {
                problems.push(format!("{code}: no docs/errors/{code}.md"));
            }
            continue;
        };
        if !md.starts_with(&format!("# {code}: ")) {
            problems.push(format!("{code}: the first line isn't `# {code}: …`"));
        }
        let bs = blocks(md);
        let checked: Vec<&String> = bs.iter().filter(|(c, _)| *c).map(|(_, t)| t).collect();
        if bs.is_empty() {
            problems.push(format!("{code}: no ```ts example"));
            continue;
        }
        if let Some(bad) = checked.first() {
            let got = reported(bad, &dir);
            if !got.contains(code) {
                problems.push(format!(
                    "{code}: the example doesn't report it (it reports {got:?}):\n{bad}"
                ));
            }
        }
        if let Some(fixed) = checked.get(1) {
            let got = reported(fixed, &dir);
            if got.contains(code) {
                problems.push(format!(
                    "{code}: the fixed example still reports it:\n{fixed}"
                ));
            }
            if code.starts_with('E') && got.iter().any(|c| c.starts_with('E')) {
                problems.push(format!(
                    "{code}: the fixed example has errors {got:?}:\n{fixed}"
                ));
            }
        }
    }
    // Explanations of codes that don't exist.
    for (c, _) in codes::EXPLANATIONS {
        if codes::parse(c).is_none() {
            problems.push(format!("docs/errors/{c}.md: no such code"));
        }
    }
    assert!(
        problems.is_empty(),
        "{} problem(s):\n\n{}",
        problems.len(),
        problems.join("\n\n")
    );
}
