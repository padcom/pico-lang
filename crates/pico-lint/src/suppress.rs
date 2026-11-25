//! Per-line suppression comments (#80), as ESLint's:
//!
//! ```ts
//! let x = 1 // pico-lint-disable-line unused
//! // pico-lint-disable-next-line naming, W0003
//! function Bad_Name() {}
//! ```
//!
//! Without rules, every lint warning of that line is dropped; with rules (names as in
//! `project.json`'s `"lint"`, or codes), those rules' warnings. A warning belongs to the line
//! its main span starts on.

use pico_diag::{Diagnostic, FileId, codes};
use std::collections::HashMap;

const SAME_LINE: &str = "pico-lint-disable-line";
const NEXT_LINE: &str = "pico-lint-disable-next-line";

/// Which rules each line suppresses (`None`: all of them), by 0-based line.
type Lines = HashMap<usize, Option<Vec<String>>>;

/// Drops the warnings of `out` that a suppression comment of `text` covers.
pub fn apply(text: &str, file: FileId, out: &mut Vec<Diagnostic>) {
    let lines = directives(text, file);
    if lines.is_empty() {
        return;
    }
    let line_of = |at: usize| text[..at.min(text.len())].matches('\n').count();
    out.retain(|d| {
        let Some(span) = d.main_span() else {
            return true;
        };
        match lines.get(&line_of(span.start as usize)) {
            None => true,
            Some(None) => false,
            Some(Some(rules)) => !d.code.is_some_and(|c| {
                let name = codes::LINTS
                    .iter()
                    .find(|(k, ..)| *k == c)
                    .map(|(_, n, _)| *n);
                rules
                    .iter()
                    .any(|r| r.eq_ignore_ascii_case(&c.to_string()) || Some(r.as_str()) == name)
            }),
        }
    });
}

/// The suppression comments of `text`: its line comments are in the gaps between tokens
/// (so `//` in a string doesn't count).
fn directives(text: &str, file: FileId) -> Lines {
    let (tokens, _) = pico_syntax::lexer::lex(file, text);
    let mut gaps = Vec::new();
    let mut prev = 0usize;
    for t in &tokens {
        let start = t.span.start as usize;
        if start > prev {
            gaps.push((prev, start));
        }
        prev = prev.max(t.span.end as usize);
    }
    gaps.push((prev, text.len()));
    let mut lines = Lines::new();
    for (a, b) in gaps {
        let gap = &text[a..b.min(text.len())];
        let mut from = 0;
        while let Some(i) = gap[from..].find("//") {
            let at = a + from + i;
            let end = text[at..].find('\n').map_or(text.len(), |n| at + n);
            let body = text[at + 2..end].trim();
            let line = text[..at].matches('\n').count();
            // (`next-line` first: `disable-line` is its suffix's prefix.)
            let (target, rest) = if let Some(r) = body.strip_prefix(NEXT_LINE) {
                (Some(line + 1), r)
            } else if let Some(r) = body.strip_prefix(SAME_LINE) {
                (Some(line), r)
            } else {
                (None, "")
            };
            if let Some(target) = target
                && (rest.is_empty() || rest.starts_with(char::is_whitespace))
            {
                // (A description may follow ` -- `, as in ESLint.)
                let rest = rest.split("--").next().unwrap_or("");
                let rules: Vec<String> = rest
                    .split([',', ' ', '\t'])
                    .filter(|r| !r.is_empty())
                    .map(str::to_string)
                    .collect();
                let entry = lines.entry(target).or_insert(Some(Vec::new()));
                match (entry.as_mut(), rules.is_empty()) {
                    (_, true) => *entry = None,
                    (Some(have), false) => have.extend(rules),
                    (None, false) => {}
                }
            }
            from = end.saturating_sub(a).max(from + i + 2);
            if from >= gap.len() {
                break;
            }
        }
    }
    lines
}
