//! The statement-termination rule (spec §1.2).
//!
//! A newline ends a statement unless:
//! - it's inside `( )`, `[ ]` or a template substitution `${ }` (inside `{ }` it counts again);
//! - the line ends with a binary operator, an assignment operator, `,`, `.`, `?.`, `:` or `=>`;
//! - the next line starts with `.`, `?.`, or an operator that can't start an expression.
//!
//! Lines starting with `-`, `+`, `(`, `[`, `!` or `~` begin a new statement. The parser may
//! still skip a terminator where the grammar can't end (after `if (…)`, before `else`, …).
//!
//! Generic closers (`>`, `>>`, `>>>`) and `?` don't continue a line when they end it, since
//! `let m: Map<K, V>` and `let s: string?` are complete statements.

use crate::token::{Kw, Punct, Token, TokenKind};

/// Drops the newlines that don't terminate a statement. The remaining [`TokenKind::Newline`]
/// tokens are statement terminators; redundant ones (doubled, after `;` or `{`, at the start)
/// are removed too.
pub fn filter(tokens: Vec<Token>) -> Vec<Token> {
    #[derive(PartialEq)]
    enum Open {
        Paren,
        Brace,
    }
    let mut out: Vec<Token> = Vec::with_capacity(tokens.len());
    let mut stack: Vec<Open> = Vec::new();
    let mut iter = tokens.into_iter().peekable();
    while let Some(tok) = iter.next() {
        match &tok.kind {
            TokenKind::Newline => {
                // Recovery: a declaration keyword can't appear inside `( )`/`[ ]`, so an
                // unclosed bracket ends at the line before it.
                if stack.last() == Some(&Open::Paren)
                    && iter.peek().is_some_and(|t| starts_declaration(&t.kind))
                {
                    while stack.last() == Some(&Open::Paren) {
                        stack.pop();
                    }
                }
                let in_parens = stack.last() == Some(&Open::Paren);
                let prev = out.last().map(|t| &t.kind);
                let redundant = matches!(
                    prev,
                    None | Some(TokenKind::Newline)
                        | Some(TokenKind::Punct(Punct::Semi | Punct::LBrace))
                );
                let cont_end = prev.is_some_and(continues_at_end);
                let cont_start = iter.peek().is_some_and(|t| continues_at_start(&t.kind));
                if !(in_parens || redundant || cont_end || cont_start) {
                    out.push(tok);
                }
                continue;
            }
            TokenKind::Punct(Punct::LParen | Punct::LBracket) | TokenKind::TemplateHead(_) => {
                stack.push(Open::Paren)
            }
            TokenKind::Punct(Punct::LBrace) => stack.push(Open::Brace),
            TokenKind::Punct(Punct::RParen | Punct::RBracket | Punct::RBrace)
            | TokenKind::TemplateTail { .. } => {
                stack.pop();
            }
            TokenKind::TemplateMiddle { .. } => {}
            _ => {}
        }
        out.push(tok);
    }
    out
}

/// Keywords that only start declarations or bindings (never an argument or element).
fn starts_declaration(kind: &TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Kw(
            Kw::Const
                | Kw::Let
                | Kw::Class
                | Kw::Struct
                | Kw::Enum
                | Kw::Interface
                | Kw::Import
                | Kw::Export
                | Kw::Extern
        )
    )
}

/// A line ending with this token continues on the next line.
fn continues_at_end(kind: &TokenKind) -> bool {
    match kind {
        TokenKind::Punct(p) => !matches!(
            p,
            Punct::RParen
                | Punct::RBracket
                | Punct::RBrace
                | Punct::Semi
                | Punct::Question
                | Punct::Bang
                | Punct::Tilde
                | Punct::PlusPlus
                | Punct::MinusMinus
                | Punct::Gt
                | Punct::Shr
                | Punct::UShr
                | Punct::At
                | Punct::LParen
                | Punct::LBracket
                | Punct::LBrace
        ),
        TokenKind::Kw(k) => matches!(k, Kw::As | Kw::Is | Kw::Instanceof),
        _ => false,
    }
}

/// A line starting with this token continues the previous line.
fn continues_at_start(kind: &TokenKind) -> bool {
    match kind {
        TokenKind::Punct(p) => matches!(
            p,
            Punct::Dot
                | Punct::QDot
                | Punct::DotDot
                | Punct::AndAnd
                | Punct::OrOr
                | Punct::QQ
                | Punct::Star
                | Punct::StarStar
                | Punct::Slash
                | Punct::Percent
                | Punct::EqEq
                | Punct::NotEq
                | Punct::Lt
                | Punct::LtEq
                | Punct::Gt
                | Punct::GtEq
                | Punct::Shl
                | Punct::Shr
                | Punct::UShr
                | Punct::And
                | Punct::Or
                | Punct::Caret
                | Punct::Question
                | Punct::Colon
                | Punct::FatArrow
                | Punct::Eq
                | Punct::PlusEq
                | Punct::MinusEq
                | Punct::StarEq
                | Punct::SlashEq
                | Punct::PercentEq
                | Punct::StarStarEq
                | Punct::ShlEq
                | Punct::ShrEq
                | Punct::UShrEq
                | Punct::AndEq
                | Punct::OrEq
                | Punct::CaretEq
                | Punct::AndAndEq
                | Punct::OrOrEq
                | Punct::QQEq
        ),
        TokenKind::Kw(k) => matches!(
            k,
            Kw::As
                | Kw::Is
                | Kw::Instanceof
                | Kw::Throws
                | Kw::Where
                | Kw::Extends
                | Kw::Implements
        ),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lex;
    use ppl_diag::FileId;

    /// Lexes and filters, then prints tokens separated by spaces, with `⏎` for terminators.
    fn show(src: &str) -> String {
        let (tokens, diags) = lex(FileId(0), src);
        assert!(diags.is_empty(), "{diags:?}");
        filter(tokens)
            .iter()
            .filter(|t| t.kind != TokenKind::Eof)
            .map(|t| match &t.kind {
                TokenKind::Ident(s) => s.clone(),
                TokenKind::Kw(k) => k.as_str().into(),
                TokenKind::Punct(p) => p.as_str().into(),
                TokenKind::Int { value, .. } => value.to_string(),
                TokenKind::Newline => "⏎".into(),
                TokenKind::Str(s) => format!("{s:?}"),
                other => other.describe(),
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[test]
    fn spec_examples() {
        let cases: &[(&str, &str)] = &[
            // Leading `.` continues the expression.
            (
                "const total = items\n  .filter(i => i.active)\n  .map(i => i.price)\n",
                "const total = items . filter ( i => i . active ) . map ( i => i . price ) ⏎",
            ),
            // `;` separates statements on one line.
            ("const a = 1; const b = 2", "const a = 1 ; const b = 2"),
            // `f\n(x)` is two statements.
            ("f\n(x)", "f ⏎ ( x )"),
            // Lines starting with `-`, `+`, `(` or `[` begin a new statement.
            ("a\n-b", "a ⏎ - b"),
            ("a\n+b", "a ⏎ + b"),
            ("a\n[1]", "a ⏎ [ 1 ]"),
            // Trailing binary operator, `,`, `.` or `=>` continues.
            ("a +\nb", "a + b"),
            ("a &&\nb", "a && b"),
            ("f(a,\nb)", "f ( a , b )"),
            ("x = a.\nb", "x = a . b"),
            ("const f = x =>\nx * 2", "const f = x => x * 2"),
            ("x =\n1", "x = 1"),
            // Leading operators that can't start an expression continue.
            ("a\n&& b", "a && b"),
            ("a\n|| b", "a || b"),
            ("a\n?? b", "a ?? b"),
            ("a\n* b", "a * b"),
            ("a\n/ b", "a / b"),
            ("a\n?.b", "a ?. b"),
        ];
        for (src, want) in cases {
            assert_eq!(&show(src), want, "for {src:?}");
        }
    }

    #[test]
    fn brackets() {
        let cases: &[(&str, &str)] = &[
            // Inside parens and brackets newlines vanish.
            ("foo(\n  a\n  b\n)", "foo ( a b )"),
            ("[\n1,\n2\n]", "[ 1 , 2 ]"),
            // Inside braces they count again, even when the braces are inside parens.
            ("{\n  a\n  b\n}", "{ a ⏎ b ⏎ }"),
            ("f(() => {\n  a\n  b\n})", "f ( ( ) => { a ⏎ b ⏎ } )"),
            // Template substitutions behave like parens.
            (
                "`${a\n+ b}`\nc",
                "template literal a + b template literal ⏎ c",
            ),
        ];
        for (src, want) in cases {
            assert_eq!(&show(src), want, "for {src:?}");
        }
    }

    #[test]
    fn complete_lines_that_end_in_operator_tokens() {
        let cases: &[(&str, &str)] = &[
            ("let m: Map<K, V>\nm = x", "let m : Map < K , V > ⏎ m = x"),
            (
                "let a: Array<Array<int>>\nb",
                "let a : Array < Array < int >> ⏎ b",
            ),
            ("let s: string?\ns = x", "let s : string ? ⏎ s = x"),
            ("x!\ny", "x ! ⏎ y"),
            ("i++\nj", "i ++ ⏎ j"),
        ];
        for (src, want) in cases {
            assert_eq!(&show(src), want, "for {src:?}");
        }
    }

    #[test]
    fn union_types_across_lines() {
        assert_eq!(
            show("type E =\n  | A\n  | B\nconst x = 1"),
            "type E = | A | B ⏎ const x = 1"
        );
    }

    #[test]
    fn redundant_newlines_are_removed() {
        assert_eq!(show("\n\na\n\n\nb\n"), "a ⏎ b ⏎");
        assert_eq!(show("a;\nb"), "a ; b");
        assert_eq!(show("x // comment\ny"), "x ⏎ y");
    }
}
