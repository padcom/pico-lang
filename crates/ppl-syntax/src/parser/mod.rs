//! A hand-written recursive-descent parser with a Pratt loop for binary operators.
//!
//! Input is the newline-filtered token stream, so a [`TokenKind::Newline`] here always means
//! "statement terminator". The parser skips terminators explicitly where a statement can't end.
//!
//! Errors are collected, and the parser recovers at statement boundaries so one file can
//! report several problems.

mod decl;
mod expr;
mod pattern;
mod stmt;
mod types;

use crate::ast::*;
use crate::token::{Kw, Punct, Token, TokenKind};
use crate::{lex, newline};
use ppl_diag::{Code, Diagnostic, FileId, Span, codes};

/// Lexes, filters newlines and parses a whole module.
pub fn parse_module(file: FileId, src: &str) -> (Module, Vec<Diagnostic>) {
    let (tokens, mut diags) = lex(file, src);
    let mut p = Parser::new(newline::filter(tokens), file);
    let module = p.module();
    diags.append(&mut p.diags);
    (module, diags)
}

/// Parses a single expression (used by tests and tools).
pub fn parse_expr(file: FileId, src: &str) -> (Expr, Vec<Diagnostic>) {
    let (tokens, mut diags) = lex(file, src);
    let mut p = Parser::new(newline::filter(tokens), file);
    let e = p.expr();
    p.skip_newlines();
    if !p.at_eof() {
        p.error_expected("end of input");
    }
    diags.append(&mut p.diags);
    (e, diags)
}

/// Saved parser state for speculative parsing.
struct Snapshot {
    pos: usize,
    diags: usize,
    undo: usize,
}

pub(crate) struct Parser {
    tokens: Vec<Token>,
    pos: usize,
    file: FileId,
    pub(crate) diags: Vec<Diagnostic>,
    /// Original tokens overwritten by `>>` splitting, so speculation can restore them.
    undo: Vec<(usize, Token)>,
    /// Position of the last reported error, to avoid cascades at one spot.
    last_error_pos: Option<usize>,
    /// Inside a match guard, `x => …` ends the guard instead of starting an arrow function.
    in_guard: bool,
}

/// Tokens a statement can start with; recovery stops before them.
pub(super) fn starts_statement(kind: &TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Kw(
            Kw::Const
                | Kw::Let
                | Kw::Function
                | Kw::Class
                | Kw::Struct
                | Kw::Enum
                | Kw::Interface
                | Kw::Import
                | Kw::Export
                | Kw::Return
                | Kw::If
                | Kw::While
                | Kw::For
                | Kw::Do
                | Kw::Loop
                | Kw::Match
                | Kw::Switch
                | Kw::Try
                | Kw::Throw
                | Kw::Extern
        )
    ) || kind.is_punct(Punct::At)
}

impl Parser {
    fn new(tokens: Vec<Token>, file: FileId) -> Parser {
        Parser {
            tokens,
            pos: 0,
            file,
            diags: Vec::new(),
            undo: Vec::new(),
            last_error_pos: None,
            in_guard: false,
        }
    }

    // ------------------------------------------------------------ token access

    fn peek(&self) -> &TokenKind {
        &self.tokens[self.pos].kind
    }

    fn peek_nth(&self, n: usize) -> &TokenKind {
        let i = (self.pos + n).min(self.tokens.len() - 1);
        &self.tokens[i].kind
    }

    fn span(&self) -> Span {
        self.tokens[self.pos].span
    }

    /// Span of the previous token (or the current one at the start).
    fn prev_span(&self) -> Span {
        self.tokens[self.pos.saturating_sub(1)].span
    }

    fn at_eof(&self) -> bool {
        matches!(self.peek(), TokenKind::Eof)
    }

    fn at(&self, p: Punct) -> bool {
        self.peek().is_punct(p)
    }

    fn at_kw(&self, k: Kw) -> bool {
        self.peek().is_kw(k)
    }

    /// The current token is the contextual word `word` (an identifier).
    fn at_word(&self, word: &str) -> bool {
        matches!(self.peek(), TokenKind::Ident(s) if s == word)
    }

    fn bump(&mut self) -> Token {
        let t = self.tokens[self.pos].clone();
        if !matches!(t.kind, TokenKind::Eof) {
            self.pos += 1;
        }
        t
    }

    fn eat(&mut self, p: Punct) -> bool {
        if self.at(p) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn eat_kw(&mut self, k: Kw) -> bool {
        if self.at_kw(k) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn eat_word(&mut self, word: &str) -> bool {
        if self.at_word(word) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn skip_newlines(&mut self) {
        while matches!(self.peek(), TokenKind::Newline) {
            self.bump();
        }
    }

    /// Skips newlines and `;` (between members, arms, …).
    fn skip_separators(&mut self) {
        while matches!(self.peek(), TokenKind::Newline) || self.at(Punct::Semi) {
            self.bump();
        }
    }

    /// Whether the next non-newline token satisfies `f`, without consuming anything.
    fn peek_past_newlines(&self, f: impl Fn(&TokenKind) -> bool) -> bool {
        let mut i = self.pos;
        while matches!(self.tokens[i].kind, TokenKind::Newline) {
            i += 1;
        }
        f(&self.tokens[i].kind)
    }

    /// Consumes newlines if the token after them satisfies `f`.
    fn eat_newlines_before(&mut self, f: impl Fn(&TokenKind) -> bool) -> bool {
        if self.peek_past_newlines(f) {
            self.skip_newlines();
            true
        } else {
            false
        }
    }

    /// Splits `>>`, `>>>`, `>=`, `>>=` and `>>>=` so a generic argument list can close.
    fn eat_gt(&mut self) -> bool {
        let rest = match self.peek() {
            TokenKind::Punct(Punct::Gt) => {
                self.bump();
                return true;
            }
            TokenKind::Punct(Punct::Shr) => Punct::Gt,
            TokenKind::Punct(Punct::UShr) => Punct::Shr,
            TokenKind::Punct(Punct::GtEq) => Punct::Eq,
            TokenKind::Punct(Punct::ShrEq) => Punct::GtEq,
            TokenKind::Punct(Punct::UShrEq) => Punct::ShrEq,
            _ => return false,
        };
        let original = self.tokens[self.pos].clone();
        self.undo.push((self.pos, original.clone()));
        let span = Span::new(self.file, original.span.start + 1, original.span.end);
        self.tokens[self.pos] = Token {
            kind: TokenKind::Punct(rest),
            span,
            newline_before: false,
        };
        true
    }

    // ------------------------------------------------------------ speculation

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            pos: self.pos,
            diags: self.diags.len(),
            undo: self.undo.len(),
        }
    }

    fn restore(&mut self, s: Snapshot) {
        self.pos = s.pos;
        self.diags.truncate(s.diags);
        while self.undo.len() > s.undo {
            let (i, t) = self.undo.pop().unwrap();
            self.tokens[i] = t;
        }
    }

    /// Runs `f`; if it fails or reports an error, rewinds and returns `None`.
    fn speculate<T>(&mut self, f: impl FnOnce(&mut Parser) -> Option<T>) -> Option<T> {
        let snap = self.snapshot();
        let saved_last = self.last_error_pos;
        match f(self) {
            Some(v) if self.diags.len() == snap.diags => Some(v),
            _ => {
                self.restore(snap);
                self.last_error_pos = saved_last;
                None
            }
        }
    }

    // ------------------------------------------------------------ errors

    pub(crate) fn report(&mut self, d: Diagnostic) {
        self.diags.push(d);
    }

    /// Reports "expected X, found Y" at the current token, unless an error was already
    /// reported at this position.
    fn error_expected(&mut self, what: &str) {
        if self.last_error_pos == Some(self.pos) {
            return;
        }
        self.last_error_pos = Some(self.pos);
        let found = self.peek().clone();
        // At a line end or EOF, point just after the previous token.
        let (span, label) = match found {
            TokenKind::Newline | TokenKind::Eof if self.pos > 0 => (
                self.prev_span().shrink_to_hi(),
                format!("expected {what} here"),
            ),
            _ => (self.span(), format!("expected {what}")),
        };
        let mut d = Diagnostic::error(
            codes::UNEXPECTED_TOKEN,
            format!("expected {what}, found {}", found.describe()),
        )
        .primary(span, label);
        if let Some(help) = suggestion(&found, what) {
            d = d.help(help);
        }
        // `const a = 1 +` ⏎ `const b = 2`: explain why the next line was read as a continuation.
        if self.tokens[self.pos].newline_before
            && self.pos > 0
            && let TokenKind::Punct(p) = &self.tokens[self.pos - 1].kind
        {
            d = d
                .secondary(self.prev_span(), "the line ends here")
                .note(format!(
                    "a line ending with `{}` continues on the next line",
                    p.as_str()
                ));
        }
        self.report(d);
    }

    fn error_at(
        &mut self,
        code: Code,
        span: Span,
        msg: impl Into<String>,
        label: impl Into<String>,
    ) {
        self.report(Diagnostic::error(code, msg).primary(span, label));
    }

    fn expect(&mut self, p: Punct) -> bool {
        if self.eat(p) {
            true
        } else {
            self.error_expected(&format!("`{}`", p.as_str()));
            false
        }
    }

    fn expect_kw(&mut self, k: Kw) -> bool {
        if self.eat_kw(k) {
            true
        } else {
            self.error_expected(&format!("`{}`", k.as_str()));
            false
        }
    }

    /// Expects the closing delimiter `close` of a list opened at `open`.
    fn expect_close(&mut self, close: Punct, open: Span) -> bool {
        if self.eat(close) {
            return true;
        }
        if self.last_error_pos == Some(self.pos) {
            return false;
        }
        self.last_error_pos = Some(self.pos);
        let found = self.peek().clone();
        let span = match found {
            TokenKind::Newline | TokenKind::Eof => self.prev_span().shrink_to_hi(),
            _ => self.span(),
        };
        let code = if matches!(found, TokenKind::Eof) {
            codes::UNCLOSED_DELIMITER
        } else {
            codes::UNEXPECTED_TOKEN
        };
        let d = Diagnostic::error(
            code,
            format!("expected `{}`, found {}", close.as_str(), found.describe()),
        )
        .primary(span, format!("expected `{}`", close.as_str()))
        .secondary(open, "to close this");
        self.report(d);
        false
    }

    fn ident(&mut self) -> Option<Ident> {
        match self.peek().clone() {
            TokenKind::Ident(name) => {
                let span = self.bump().span;
                Some(Ident { name, span })
            }
            _ => {
                self.error_expected("an identifier");
                None
            }
        }
    }

    /// An identifier or a reserved word, where property names are allowed (`map.get`).
    fn property_name(&mut self) -> Option<Ident> {
        match self.peek().clone() {
            TokenKind::Ident(name) => {
                let span = self.bump().span;
                Some(Ident { name, span })
            }
            TokenKind::Kw(k) => {
                let span = self.bump().span;
                Some(Ident {
                    name: k.as_str().to_string(),
                    span,
                })
            }
            _ => {
                self.error_expected("a property name");
                None
            }
        }
    }

    fn error_ident(&self) -> Ident {
        Ident {
            name: "<error>".into(),
            span: self.span(),
        }
    }

    /// Skips tokens until a likely statement boundary: a terminator, a `}` that closes the
    /// enclosing block, or a token that starts a statement.
    fn synchronize(&mut self) {
        let mut depth: i32 = 0;
        loop {
            match self.peek() {
                TokenKind::Eof => return,
                TokenKind::Newline | TokenKind::Punct(Punct::Semi) if depth == 0 => {
                    self.bump();
                    return;
                }
                TokenKind::Punct(Punct::RBrace) if depth == 0 => return,
                TokenKind::Punct(Punct::LBrace | Punct::LParen | Punct::LBracket) => depth += 1,
                TokenKind::Punct(Punct::RBrace | Punct::RParen | Punct::RBracket) => {
                    depth = (depth - 1).max(0)
                }
                k if depth == 0 && starts_statement(k) && self.tokens[self.pos].newline_before => {
                    return;
                }
                _ => {}
            }
            self.bump();
        }
    }

    // ------------------------------------------------------------ module

    fn module(&mut self) -> Module {
        let start = self.span();
        let stmts = self.stmt_list(|p| p.at_eof());
        let span = start.to(self.span());
        Module { stmts, span }
    }
}

/// Suggestions for common mistakes.
fn suggestion(found: &TokenKind, expected: &str) -> Option<String> {
    match found {
        TokenKind::Ident(s) if s == "var" => {
            Some("ppl has no `var`; use `let` (reassignable) or `const`".into())
        }
        TokenKind::Kw(Kw::In) => {
            Some("there's no `for-in`; use `for (const k of obj.keys())`".into())
        }
        TokenKind::Punct(Punct::Eq) if expected.contains("expression") => {
            Some("ppl has no `===`; `==` is already strict".into())
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests;
