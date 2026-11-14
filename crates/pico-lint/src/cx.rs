//! What the rules share: the file's syntax tree and text, its tokens and comments, the types
//! of its expressions (when it checked), and a way to make a rule's warning.

use crate::config::{Config, Setting};
use pico_diag::{Diagnostic, FileId, Span, codes};
use pico_driver::Checked;
use pico_sema::ty::Ty;
use pico_syntax::ast::Module;
use pico_syntax::token::{Token, TokenKind};
use std::collections::HashMap;

pub struct Cx<'a> {
    pub module: &'a Module,
    pub text: &'a str,
    pub file: FileId,
    pub checked: &'a Checked,
    pub cfg: &'a Config,
    /// Tokens without line breaks and the end.
    pub tokens: Vec<Token>,
    /// Comments: their spans (`//` to the line's end, `/*` to `*/`).
    pub comments: Vec<Span>,
    line_starts: Vec<u32>,
    types: HashMap<Span, &'a Ty>,
}

impl<'a> Cx<'a> {
    pub fn new(
        module: &'a Module,
        text: &'a str,
        file: FileId,
        checked: &'a Checked,
        cfg: &'a Config,
    ) -> Cx<'a> {
        let (tokens, _) = pico_syntax::lexer::lex(file, text);
        let tokens: Vec<Token> = tokens
            .into_iter()
            .filter(|t| !matches!(t.kind, TokenKind::Newline | TokenKind::Eof))
            .collect();
        let comments = find_comments(text, file, &tokens);
        let mut line_starts = vec![0];
        line_starts.extend(
            text.bytes()
                .enumerate()
                .filter(|(_, b)| *b == b'\n')
                .map(|(i, _)| i as u32 + 1),
        );
        let mut types = HashMap::new();
        if let (Some(p), Some(t)) = (&checked.hir, &checked.types) {
            for (span, ty) in p.expr_spans.iter().zip(&t.exprs) {
                if span.file == file {
                    types.entry(*span).or_insert(ty);
                }
            }
        }
        Cx {
            module,
            text,
            file,
            checked,
            cfg,
            tokens,
            comments,
            line_starts,
            types,
        }
    }

    pub fn on(&self, rule: &str) -> Option<Setting> {
        self.cfg.on(rule)
    }

    /// The type of the expression at `span`, when the file checked.
    pub fn ty(&self, span: Span) -> Option<&'a Ty> {
        self.types.get(&span).copied()
    }

    /// The 0-based line of byte `offset`.
    pub fn line(&self, offset: u32) -> usize {
        self.line_starts.partition_point(|&s| s <= offset) - 1
    }

    /// The text of line `l` (0-based), without its line break.
    pub fn line_text(&self, l: usize) -> &'a str {
        let start = self.line_starts[l] as usize;
        let end = self
            .line_starts
            .get(l + 1)
            .map(|&e| e as usize - 1)
            .unwrap_or(self.text.len());
        &self.text[start..end]
    }

    pub fn line_count(&self) -> usize {
        self.line_starts.len()
    }

    pub fn line_start(&self, l: usize) -> u32 {
        self.line_starts[l]
    }

    pub fn src(&self, span: Span) -> &'a str {
        &self.text[span.start as usize..span.end as usize]
    }

    pub fn span(&self, start: u32, end: u32) -> Span {
        Span::new(self.file, start, end)
    }

    /// The index of the first token starting at or after `offset`.
    pub fn token_at(&self, offset: u32) -> usize {
        self.tokens.partition_point(|t| t.span.start < offset)
    }

    /// The last token ending at or before `offset`.
    pub fn token_before(&self, offset: u32) -> Option<&Token> {
        let i = self.tokens.partition_point(|t| t.span.end <= offset);
        i.checked_sub(1).map(|i| &self.tokens[i])
    }

    /// The first token between `from` and `to` that is `kind`.
    pub fn find_token(
        &self,
        from: u32,
        to: u32,
        pred: impl Fn(&TokenKind) -> bool,
    ) -> Option<&Token> {
        self.tokens[self.token_at(from)..]
            .iter()
            .take_while(|t| t.span.end <= to)
            .find(|t| pred(&t.kind))
    }

    /// Whether there's a comment between `from` and `to`.
    pub fn has_comment(&self, from: u32, to: u32) -> bool {
        self.comments.iter().any(|c| c.start >= from && c.end <= to)
    }

    /// A warning of rule `rule` at `span`.
    pub fn warn(&self, rule: &str, msg: impl Into<String>, span: Span, label: &str) -> Diagnostic {
        Diagnostic::warning(msg.into())
            .with_code(codes::lint_code(rule).expect("a known rule"))
            .primary(span, label.to_string())
    }
}

/// The comments in the gaps between tokens.
fn find_comments(text: &str, file: FileId, tokens: &[Token]) -> Vec<Span> {
    let mut out = Vec::new();
    let mut gap_start = 0usize;
    let scan = |from: usize, to: usize, out: &mut Vec<Span>| {
        let b = text.as_bytes();
        let mut i = from;
        while i + 1 < to {
            if b[i] == b'/' && b[i + 1] == b'/' {
                let end = text[i..to].find('\n').map_or(to, |n| i + n);
                out.push(Span::new(file, i as u32, end as u32));
                i = end;
            } else if b[i] == b'/' && b[i + 1] == b'*' {
                let end = text[i + 2..to].find("*/").map_or(to, |n| i + 2 + n + 2);
                out.push(Span::new(file, i as u32, end as u32));
                i = end;
            } else {
                i += 1;
            }
        }
    };
    for t in tokens {
        scan(gap_start, t.span.start as usize, &mut out);
        gap_start = t.span.end as usize;
    }
    scan(gap_start, text.len(), &mut out);
    out
}
