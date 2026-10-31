//! Source map, spans and diagnostics rendering for the ppl compiler.

pub mod codes;
mod diagnostic;
mod render;
mod source;

pub use codes::Code;
pub use diagnostic::{Diagnostic, Label, Severity};
pub use render::{render, render_all};
pub use source::{FileId, SourceFile, SourceMap, Span};
