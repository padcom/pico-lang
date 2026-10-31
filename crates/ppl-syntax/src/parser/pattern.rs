//! Patterns (spec §3.5, §4.1).

use super::Parser;
use crate::ast::*;
use crate::token::{Kw, Punct, TokenKind};
use ppl_diag::Span;

/// Where a pattern appears; this decides how `name:` and `name {` are read.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum PatCtx {
    /// `const`/`let`, `for-of`, parameters: identifiers and destructuring only. The `:` after
    /// a name belongs to the type annotation.
    Binding,
    /// `match` arms, `is`, `catch`: the full pattern language, `name: Type` is a type test.
    Match,
    /// `case …:` — like `Match`, but a top-level `name:` ends the pattern.
    Case,
}

fn mk(kind: PatternKind, span: Span) -> Pattern {
    Pattern { kind, span }
}

impl Parser {
    pub(crate) fn binding_pattern(&mut self) -> Pattern {
        self.pattern(PatCtx::Binding)
    }

    /// A pattern with or-alternatives (`a | b`), except in binding position.
    pub(crate) fn pattern(&mut self, ctx: PatCtx) -> Pattern {
        let start = self.span();
        if ctx == PatCtx::Binding {
            return self.single_pattern(ctx);
        }
        self.eat(Punct::Or);
        let first = self.single_pattern(ctx);
        if !self.at(Punct::Or) {
            return first;
        }
        let mut alts = vec![first];
        while self.eat(Punct::Or) {
            alts.push(self.single_pattern(ctx));
        }
        mk(PatternKind::Or(alts), start.to(self.prev_span()))
    }

    fn single_pattern(&mut self, ctx: PatCtx) -> Pattern {
        let start = self.span();
        let binding = ctx == PatCtx::Binding;
        match self.peek().clone() {
            TokenKind::Ident(name) if name == "_" => {
                self.bump();
                mk(PatternKind::Wildcard, start)
            }
            TokenKind::Punct(Punct::LBracket) => self.array_pattern(ctx),
            // `(pattern)`: grouping, e.g. `case (s: string):` (spec §3.5).
            TokenKind::Punct(Punct::LParen) if !binding => {
                let open = self.bump().span;
                let inner = self.pattern(PatCtx::Match);
                self.expect_close(Punct::RParen, open);
                Pattern {
                    kind: inner.kind,
                    span: open.to(self.prev_span()),
                }
            }
            TokenKind::Punct(Punct::LBrace) => {
                let (fields, rest) = self.field_patterns(ctx);
                mk(
                    PatternKind::Struct {
                        path: None,
                        fields,
                        rest,
                    },
                    start.to(self.prev_span()),
                )
            }
            TokenKind::Ident(_) if binding => {
                let name = self.ident().unwrap();
                mk(PatternKind::Ident(name), start)
            }
            TokenKind::Ident(_) => self.ident_pattern(ctx),
            _ if !binding => {
                let Some(lit) = self.literal_pattern() else {
                    self.error_expected("a pattern");
                    if !matches!(
                        self.peek(),
                        TokenKind::Newline
                            | TokenKind::Eof
                            | TokenKind::Punct(Punct::RBrace | Punct::RParen)
                    ) {
                        self.bump();
                    }
                    return mk(PatternKind::Error, start);
                };
                if self.eat(Punct::DotDot) {
                    let end = match self.literal_pattern() {
                        Some(e) => e,
                        None => {
                            self.error_expected("the end of the range");
                            mk(PatternKind::Error, self.span())
                        }
                    };
                    let span = start.to(self.prev_span());
                    return mk(
                        PatternKind::Range {
                            start: Box::new(lit),
                            end: Box::new(end),
                        },
                        span,
                    );
                }
                lit
            }
            _ => {
                self.error_expected("a binding name or a destructuring pattern");
                mk(PatternKind::Error, start)
            }
        }
    }

    /// `42`, `-1`, `"north"`, `true`, `null`.
    fn literal_pattern(&mut self) -> Option<Pattern> {
        let start = self.span();
        let neg = self.at(Punct::Minus)
            && matches!(
                self.peek_nth(1),
                TokenKind::Int { .. } | TokenKind::Float { .. }
            );
        if neg {
            self.bump();
        }
        let lit = match self.peek().clone() {
            TokenKind::Int { value, suffix } => Lit::Int { value, suffix },
            TokenKind::Float { value, suffix } => Lit::Float { value, suffix },
            TokenKind::Str(s) => Lit::Str(s),
            TokenKind::Kw(Kw::True) => Lit::Bool(true),
            TokenKind::Kw(Kw::False) => Lit::Bool(false),
            TokenKind::Kw(Kw::Null) => Lit::Null,
            _ => return None,
        };
        let lit_span = self.bump().span;
        let pat = mk(PatternKind::Lit(lit), lit_span);
        if neg {
            return Some(mk(PatternKind::Neg(Box::new(pat)), start.to(lit_span)));
        }
        Some(pat)
    }

    /// Patterns starting with a name: bindings, variants, struct patterns, type tests, `as`.
    fn ident_pattern(&mut self, ctx: PatCtx) -> Pattern {
        let start = self.span();
        let first = self.ident().unwrap();
        // `name as Pattern`
        if self.at_kw(Kw::As) {
            self.bump();
            let pat = self.single_pattern(ctx);
            let span = start.to(self.prev_span());
            return mk(
                PatternKind::As {
                    name: first,
                    pat: Box::new(pat),
                },
                span,
            );
        }
        // `name: Type` (type test), optionally with `{ fields }`.
        if self.at(Punct::Colon) && ctx == PatCtx::Match {
            self.bump();
            let ty = self.ty_no_union();
            let fields = if self.at(Punct::LBrace) {
                let (fields, _rest) = self.field_patterns(ctx);
                Some(fields)
            } else {
                None
            };
            let span = start.to(self.prev_span());
            return mk(
                PatternKind::Typed {
                    name: first,
                    ty,
                    fields,
                },
                span,
            );
        }
        let mut path = vec![first];
        while self.at(Punct::Dot) {
            self.bump();
            match self.ident() {
                Some(i) => path.push(i),
                None => break,
            }
        }
        if self.at(Punct::LParen) {
            let open = self.bump().span;
            let mut elems = Vec::new();
            while !self.at(Punct::RParen) && !self.at_eof() {
                elems.push(self.pattern(ctx));
                if !self.eat(Punct::Comma) {
                    break;
                }
            }
            self.expect_close(Punct::RParen, open);
            let span = start.to(self.prev_span());
            return mk(PatternKind::TupleStruct { path, elems }, span);
        }
        if self.at(Punct::LBrace) {
            let (fields, rest) = self.field_patterns(ctx);
            let span = start.to(self.prev_span());
            return mk(
                PatternKind::Struct {
                    path: Some(path),
                    fields,
                    rest,
                },
                span,
            );
        }
        let span = start.to(self.prev_span());
        if path.len() == 1 {
            mk(PatternKind::Ident(path.pop().unwrap()), span)
        } else {
            mk(PatternKind::Path(path), span)
        }
    }

    /// `{ x, y: pat, z = default, ...rest }`
    fn field_patterns(&mut self, ctx: PatCtx) -> (Vec<FieldPat>, Option<Option<Ident>>) {
        let open = self.bump().span; // {
        let sub_ctx = if ctx == PatCtx::Binding {
            PatCtx::Binding
        } else {
            PatCtx::Match
        };
        let mut fields = Vec::new();
        let mut rest = None;
        loop {
            self.skip_newlines();
            if self.at(Punct::RBrace) || self.at_eof() {
                break;
            }
            if self.eat(Punct::Ellipsis) {
                rest = Some(if matches!(self.peek(), TokenKind::Ident(_)) {
                    self.ident()
                } else {
                    None
                });
            } else {
                let fstart = self.span();
                let Some(name) = self.property_name() else {
                    break;
                };
                let pat = if self.eat(Punct::Colon) {
                    Some(self.pattern(sub_ctx))
                } else {
                    None
                };
                let default = if self.eat(Punct::Eq) {
                    Some(self.expr())
                } else {
                    None
                };
                fields.push(FieldPat {
                    name,
                    pat,
                    default,
                    span: fstart.to(self.prev_span()),
                });
            }
            self.skip_newlines();
            if !self.eat(Punct::Comma) {
                break;
            }
        }
        self.skip_newlines();
        self.expect_close(Punct::RBrace, open);
        (fields, rest)
    }

    /// `[a, b = 1, ...rest]`
    fn array_pattern(&mut self, ctx: PatCtx) -> Pattern {
        let open = self.bump().span;
        let sub_ctx = if ctx == PatCtx::Binding {
            PatCtx::Binding
        } else {
            PatCtx::Match
        };
        let mut elems = Vec::new();
        while !self.at(Punct::RBracket) && !self.at_eof() {
            let start = self.span();
            if self.eat(Punct::Ellipsis) {
                let name = if matches!(self.peek(), TokenKind::Ident(_)) {
                    self.ident()
                } else {
                    None
                };
                elems.push(mk(PatternKind::Rest(name), start.to(self.prev_span())));
            } else {
                let p = self.pattern(sub_ctx);
                if self.at(Punct::Eq) {
                    self.bump();
                    let d = self.expr();
                    let span = start.to(self.prev_span());
                    elems.push(mk(
                        PatternKind::Default {
                            pat: Box::new(p),
                            default: Box::new(d),
                        },
                        span,
                    ));
                } else {
                    elems.push(p);
                }
            }
            if !self.eat(Punct::Comma) {
                break;
            }
        }
        self.expect_close(Punct::RBracket, open);
        mk(PatternKind::Array(elems), open.to(self.prev_span()))
    }
}
