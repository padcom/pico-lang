//! Compilation pipeline, caching, targets and linking for ppl.
//!
//! [`check_source`] runs the front end: parsing, then name resolution. Later stages plug in
//! there as they're built, and [`Stage::IMPLEMENTED`] tells tools which diagnostics can
//! currently be produced. Each stage runs only if the previous ones reported no errors.

use ppl_diag::{Code, Diagnostic, SourceMap};
use ppl_syntax::ast::Module;
use std::path::Path;

/// Compiler stages, in pipeline order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Stage {
    /// Lexing and parsing (`E00xx`, `E01xx`).
    Parse,
    /// Name resolution and modules (`E02xx`).
    Resolve,
    /// Type checking (`E03xx` and above).
    Typecheck,
}

impl Stage {
    /// The stages the compiler runs today.
    pub const IMPLEMENTED: &'static [Stage] = &[Stage::Parse, Stage::Resolve];

    pub fn is_implemented(self) -> bool {
        Stage::IMPLEMENTED.contains(&self)
    }

    /// The stage that reports `code` (from the ranges in `ppl_diag::codes`).
    pub fn of(code: Code) -> Stage {
        match code.0 {
            0..=199 => Stage::Parse,
            200..=299 => Stage::Resolve,
            _ => Stage::Typecheck,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Stage::Parse => "parser",
            Stage::Resolve => "name resolution",
            Stage::Typecheck => "type checker",
        }
    }
}

/// The result of running the front end on one file.
pub struct Checked {
    pub sm: SourceMap,
    pub module: Module,
    /// The resolved program, if parsing succeeded.
    pub hir: Option<ppl_sema::hir::Program>,
    pub diags: Vec<Diagnostic>,
}

impl Checked {
    pub fn has_errors(&self) -> bool {
        self.diags.iter().any(Diagnostic::is_error)
    }

    pub fn render_diagnostics(&self) -> String {
        ppl_diag::render_all(&self.diags, &self.sm)
    }
}

/// Runs every implemented front-end stage on `text`.
pub fn check_source(name: impl Into<String>, text: impl Into<String>) -> Checked {
    let mut sm = SourceMap::new();
    let file = sm.add(name, text);
    let (module, mut diags) = ppl_syntax::parse_module(file, &sm.file(file).text);
    let mut hir = None;
    if !diags.iter().any(Diagnostic::is_error) {
        let (program, mut d) = ppl_sema::resolve(&module, file);
        diags.append(&mut d);
        hir = Some(program);
    }
    // Report in source order, whichever pass found each problem.
    diags.sort_by_key(|d| d.main_span().map(|s| (s.file, s.start)));
    Checked {
        sm,
        module,
        hir,
        diags,
    }
}

/// Reads `path` and runs [`check_source`] on it, naming the file by its path.
pub fn check_file(path: &Path) -> std::io::Result<Checked> {
    let text = std::fs::read_to_string(path)?;
    Ok(check_source(path.display().to_string(), text))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ppl_diag::codes;

    #[test]
    fn stages_of_codes() {
        assert_eq!(Stage::of(codes::UNTERMINATED_STRING), Stage::Parse);
        assert_eq!(Stage::of(codes::UNEXPECTED_TOKEN), Stage::Parse);
        assert_eq!(Stage::of(codes::MISMATCHED_TYPES), Stage::Typecheck);
        assert!(Stage::Parse.is_implemented());
        assert!(Stage::Resolve.is_implemented());
        assert!(!Stage::Typecheck.is_implemented());
    }

    #[test]
    fn check_reports_syntax_errors() {
        let c = check_source("t.ppl", "const = 1\n");
        assert!(c.has_errors());
        assert!(c.render_diagnostics().contains("t.ppl:1:7"));
        assert!(!check_source("ok.ppl", "const a = 1\n").has_errors());
    }
}
