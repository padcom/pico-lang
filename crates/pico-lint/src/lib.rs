//! The pico linter (`pico lint`, spec §14.7): warnings about naming, unused code, complexity,
//! blank lines, documentation, likely mistakes, simplifications and style, configured in
//! `project.json`'s `"lint"`.
//!
//! The rules read a file's syntax tree (re-parsed from its text, so they see only that file),
//! and some also the checked program (types, uses). A file with syntax errors isn't linted
//! (the errors come first); one with other errors is, without what needs the checked program.

mod asyncs;
mod blank;
mod complexity;
pub mod config;
mod cx;
mod docs;
mod doctags;
mod mistakes;
mod naming;
mod returns;
mod scope;
mod simplify;
mod sizes;
mod style;
mod suppress;
mod unused;
pub(crate) use pico_syntax::walk;

pub use config::Config;
use pico_diag::{Diagnostic, FileId};
use pico_driver::Checked;
use std::path::Path;

/// The lint warnings of file `file` of a check, under `cfg`.
pub fn lint(checked: &Checked, file: FileId, cfg: &Config) -> Vec<Diagnostic> {
    let text = &checked.sm.file(file).text;
    let (module, diags) = pico_syntax::parse_module(file, text);
    if diags.iter().any(Diagnostic::is_error) {
        return Vec::new();
    }
    let mut out = Vec::new();
    if cfg.naming {
        naming::check(&module, &mut out);
    }
    if cfg.unused {
        unused::check(&module, text, file, checked, &mut out);
    }
    if let Some(max) = cfg.complexity {
        complexity::check(&module, text, max, &mut out);
    }
    if cfg.blank_lines.is_some() {
        blank::check(&module, text, cfg, &mut out);
    }
    if cfg.docs {
        docs::check(&module, text, file, checked, &mut out);
    }
    if cfg.on("explicitReturn").is_some() {
        returns::check(&module, text, &mut out);
    }
    let cx = cx::Cx::new(&module, text, file, checked, cfg);
    sizes::check(&cx, &mut out);
    mistakes::check(&cx, &mut out);
    asyncs::check(&cx, &mut out);
    simplify::check(&cx, &mut out);
    style::check(&cx, &mut out);
    doctags::check(&cx, &mut out);
    suppress::apply(text, file, &mut out);
    out.sort_by_key(|d| d.main_span().map(|s| (s.start, s.end)));
    out
}

/// `text` with the fixes of `warnings` (in `file`) applied (`pico lint --fix`, #80), or `None`
/// when there are none. Edits that overlap an earlier one are left for the next round (the
/// caller lints again).
pub fn apply_fixes(text: &str, file: FileId, warnings: &[Diagnostic]) -> Option<String> {
    let mut edits: Vec<(usize, usize, &str)> = warnings
        .iter()
        .flat_map(|d| &d.fixes)
        .filter(|(s, _)| s.file == file && (s.end as usize) <= text.len())
        .map(|(s, t)| (s.start as usize, s.end as usize, t.as_str()))
        .collect();
    edits.sort_by_key(|e| (e.0, e.1));
    let mut kept: Vec<(usize, usize, &str)> = Vec::new();
    for e in edits {
        if kept.last().is_none_or(|k| k.1 <= e.0) {
            kept.push(e);
        }
    }
    if kept.is_empty() {
        return None;
    }
    let mut out = text.to_string();
    for (a, b, t) in kept.into_iter().rev() {
        out.replace_range(a..b, t);
    }
    Some(out)
}

/// The file a check was made for (its entry: the first file added).
pub fn entry_file(checked: &Checked) -> FileId {
    checked
        .hir
        .as_ref()
        .map(|h| h.file)
        .or_else(|| checked.sm.ids().next())
        .unwrap_or(FileId(0))
}

/// Checks and lints the file at `path` with `text`, under the configuration of its directory
/// (`cfg`, with the overrides that match the file applied): the check (its errors, and the
/// source map for rendering) and the warnings.
pub fn lint_file(path: &Path, text: &str, cfg: &Config) -> (Checked, Vec<Diagnostic>) {
    let dir = path.parent().unwrap_or(Path::new("."));
    let checked = pico_driver::check_source_for_lint(
        path.display().to_string(),
        text.to_string(),
        dir,
        pico_driver::BuildOptions::default(),
    );
    let cfg = cfg.for_path(path);
    let warnings = lint(&checked, entry_file(&checked), &cfg);
    (checked, warnings)
}

#[cfg(test)]
mod tests;
