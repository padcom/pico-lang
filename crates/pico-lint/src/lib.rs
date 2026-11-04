//! The pico linter (`pico lint`, spec §14.7): warnings about naming, unused code, complexity,
//! blank lines and documentation, configured in `project.json`'s `"lint"`.
//!
//! The rules read a file's syntax tree (re-parsed from its text, so they see only that file),
//! and `unused` and `docs` also the checked program. A file with syntax errors isn't linted
//! (the errors come first); one with other errors is, without what needs the checked program.

mod blank;
mod complexity;
pub mod config;
mod docs;
mod naming;
mod unused;
mod walk;

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
    out.sort_by_key(|d| d.main_span().map(|s| (s.start, s.end)));
    out
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

/// Checks and lints the file at `path` with `text`, under the configuration of its directory:
/// the check (its errors, and the source map for rendering) and the warnings.
pub fn lint_file(path: &Path, text: &str, cfg: &Config) -> (Checked, Vec<Diagnostic>) {
    let dir = path.parent().unwrap_or(Path::new("."));
    let checked = pico_driver::check_source_with(
        path.display().to_string(),
        text.to_string(),
        dir,
        pico_driver::BuildOptions::default(),
    );
    let warnings = lint(&checked, entry_file(&checked), cfg);
    (checked, warnings)
}

#[cfg(test)]
mod tests;
