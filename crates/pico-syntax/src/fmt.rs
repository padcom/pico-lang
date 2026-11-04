//! `pico fmt` (spec §14.4, decision 265): a formatter that works on the token stream, so it's
//! lossless: comments and the text of every token are kept, only the whitespace between tokens
//! is rewritten.
//!
//! - Indentation follows bracket nesting (`{`, `(`, `[`, template substitutions), one extra
//!   level for continuation lines (a line starting with `.`/`?.` or a binary operator, or
//!   following a line that ends with one or with `=`).
//! - On a line: no space before `,`, `;`, `)`, `]`, none after `(`, `[`, one after `,`; other
//!   runs of spaces become one (spacing before a trailing comment is kept).
//! - At most one blank line in a row, none at the start or before a closing `}`, no trailing
//!   whitespace, `\n` line ends and a final newline.
//! - `'single'` strings become `"double"` (with `quotes: "double"`) when that needs no new
//!   escapes.
//!
//! Tokens spanning several lines (multi-line strings and templates, block comments) are kept
//! verbatim. Long lines aren't broken yet (`lineWidth`), and semicolons are never added or
//! removed: a trailing `;` changes what a block evaluates to.

use crate::token::{Punct, TokenKind};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Quotes {
    Double,
    Single,
}

/// The `fmt` options of `project.json` (spec §14.4).
#[derive(Clone, Copy, Debug)]
pub struct FmtOptions {
    pub indent: usize,
    pub line_width: usize,
    pub quotes: Quotes,
    pub semicolons: bool,
}

impl Default for FmtOptions {
    fn default() -> Self {
        FmtOptions {
            indent: 2,
            line_width: 100,
            quotes: Quotes::Double,
            semicolons: false,
        }
    }
}

/// Formats a source file. A file that doesn't lex is an error (with the first problem).
pub fn format(src: &str, opts: FmtOptions) -> Result<String, String> {
    let src = src.replace("\r\n", "\n");
    let (tokens, diags) = crate::lexer::lex(pico_diag::FileId(0), &src);
    if let Some(d) = diags.iter().find(|d| d.is_error()) {
        return Err(d.message.clone());
    }
    let toks: Vec<(TokenKind, usize, usize)> = tokens
        .into_iter()
        .filter(|t| !matches!(t.kind, TokenKind::Newline | TokenKind::Eof))
        .map(|t| (t.kind, t.span.start as usize, t.span.end as usize))
        .collect();
    let mut out = String::new();
    // Open brackets, each with the indentation of the line it was opened on.
    let mut stack: Vec<usize> = Vec::new();
    let mut line_indent: usize = 0;
    // The first token of the previous code line (`if`/`else` or not) and its indentation, and
    // this line's.
    let mut prev_line_first: Option<(bool, usize)> = None;
    let mut this_line_first: Option<(bool, usize)> = None;
    let mut prev: Option<usize> = None; // index into toks
    let mut line_start = true;
    let mut prev_line_last: Option<usize> = None; // last token of the previous code line
    let mut pos = 0;
    for i in 0..=toks.len() {
        let next_start = toks.get(i).map_or(src.len(), |t| t.1);
        let gap = &src[pos..next_start];
        // The gap: whitespace and comments.
        let mut newlines = 0usize;
        let mut pieces: Vec<(usize, &str)> = Vec::new(); // (newlines before, comment)
        let mut k = 0;
        let g = gap.as_bytes();
        let mut spaces = 0usize;
        let mut nl_before_piece = 0usize;
        let mut spaces_before_first = 0usize;
        while k < g.len() {
            match g[k] {
                b'\n' => {
                    newlines += 1;
                    nl_before_piece += 1;
                    spaces = 0;
                    k += 1;
                }
                b' ' | b'\t' | b'\r' => {
                    spaces += 1;
                    k += 1;
                }
                b'/' if g.get(k + 1) == Some(&b'/') => {
                    let end = gap[k..].find('\n').map_or(g.len(), |e| k + e);
                    if pieces.is_empty() {
                        spaces_before_first = spaces;
                    }
                    // (Verbatim: trailing spaces in a comment may matter, `// expect: a `.)
                    pieces.push((nl_before_piece, &gap[k..end]));
                    nl_before_piece = 0;
                    k = end;
                }
                b'/' if g.get(k + 1) == Some(&b'*') => {
                    let end = gap[k + 2..].find("*/").map_or(g.len(), |e| k + 2 + e + 2);
                    if pieces.is_empty() {
                        spaces_before_first = spaces;
                    }
                    pieces.push((nl_before_piece, &gap[k..end]));
                    newlines += gap[k..end].matches('\n').count();
                    nl_before_piece = 0;
                    k = end;
                }
                _ => {
                    k += 1;
                }
            }
        }
        let tok = toks.get(i);
        let closer = tok.is_some_and(|t| closes(&t.0));
        // A gap holding more than whitespace and comments (a template's format spec, `:x`)
        // is part of the code: kept as it is.
        if gap.chars().any(|c| !c.is_whitespace()) && pieces.is_empty() {
            out.push_str(gap);
            if let Some(&(ref kind, start, end)) = tok {
                out.push_str(&src[start..end]);
                stack_update(&mut stack, kind, &mut line_indent);
                prev = Some(i);
                pos = end;
                line_start = false;
                continue;
            }
            break;
        }
        // Comments first.
        for (nl, c) in &pieces {
            if prev.is_none() && out.is_empty() {
                // At the start of the file.
            } else if *nl == 0 && !line_start {
                // A comment after code on the same line: its spacing is kept.
                out.push_str(&" ".repeat(spaces_before_first.max(1)));
            } else {
                if !line_start {
                    out.push('\n');
                }
                if *nl > 1 && !out.is_empty() {
                    out.push('\n');
                }
                line_start = true;
            }
            if line_start {
                let d = stack.last().map_or(0, |l| l + 1);
                out.push_str(&" ".repeat(d * opts.indent));
            }
            out.push_str(c);
            line_start = false;
            if c.starts_with("//") {
                // A line comment ends its line.
                out.push('\n');
                line_start = true;
            }
        }
        let Some(&(ref kind, start, end)) = tok else {
            break;
        };
        // Line breaks before the token.
        if prev.is_some() && newlines > 0 && !line_start {
            out.push('\n');
            line_start = true;
        }
        if !out.is_empty() && line_start {
            // A blank line kept (one at most), but not right before a closing bracket.
            // (After comments: the line breaks that followed the last one.)
            let blank = if pieces.is_empty() {
                newlines > 1
            } else {
                nl_before_piece > 1
            };
            if blank && !closer && !out.ends_with("\n\n") && !out.ends_with("{\n") {
                out.push('\n');
            }
        }
        if line_start {
            let mut d = stack.last().map_or(0, |l| l + 1);
            if closer {
                d = stack.last().copied().unwrap_or(0);
            }
            let is_else = matches!(kind, TokenKind::Kw(crate::token::Kw::Else));
            let cont = prev.is_some()
                && !closer
                && !is_else
                && (continues_line(kind)
                    || prev_line_last
                        .is_some_and(|p| ends_line_open(&toks[p].0, &src[toks[p].1..toks[p].2])));
            if cont && !opens_on(&toks, prev_line_last) {
                d += 1;
            }
            // An `else` line of an expression chain: as the `if`/`else` line before it, or one
            // level past a line where the `if` came later (`const v = if (…) a`).
            if is_else && let Some((first_kw, ind)) = prev_line_first {
                d = if first_kw { ind } else { ind + 1 };
            }
            this_line_first = Some((
                matches!(
                    kind,
                    TokenKind::Kw(crate::token::Kw::If | crate::token::Kw::Else)
                ),
                d,
            ));
            out.push_str(&" ".repeat(d * opts.indent));
            line_indent = d;
        } else if let Some(p) = prev {
            // Spacing on the line.
            let pk = &toks[p].0;
            let none = gap.is_empty()
                || matches!(
                    kind,
                    TokenKind::Punct(Punct::Comma | Punct::Semi | Punct::RParen | Punct::RBracket)
                )
                || matches!(pk, TokenKind::Punct(Punct::LParen | Punct::LBracket));
            if matches!(pk, TokenKind::Punct(Punct::Comma)) || !none {
                out.push(' ');
            }
        }
        // The token, its text as written (strings may change quotes).
        let text = &src[start..end];
        match kind {
            TokenKind::Str(_) if opts.quotes == Quotes::Double => {
                out.push_str(&double_quoted(text))
            }
            _ => out.push_str(text),
        }
        line_start = false;
        stack_update(&mut stack, kind, &mut line_indent);
        if let Some(next) = toks.get(i + 1)
            && src[end..next.1].contains('\n')
        {
            prev_line_last = Some(i);
            prev_line_first = this_line_first;
        }
        prev = Some(i);
        pos = end;
    }
    // Trailing whitespace, then exactly one final newline.
    // (Not after a line comment, whose text is kept as it is.)
    let mut lines: Vec<&str> = out
        .lines()
        .map(|l| if l.contains("//") { l } else { l.trim_end() })
        .collect();
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    while lines.first().is_some_and(|l| l.is_empty()) {
        lines.remove(0);
    }
    let mut text = lines.join("\n");
    text.push('\n');
    Ok(text)
}

/// Brackets opened on one line all indent their contents one level past that line.
/// `line_indent` is the level brackets opened next on the line take: closing a bracket opened
/// on an earlier line brings it back to that bracket's level (`noBody: bool): T {`).
fn stack_update(stack: &mut Vec<usize>, kind: &TokenKind, line_indent: &mut usize) {
    if opens(kind) {
        // (Several on a line count once: their contents share the next level.)
        stack.push(*line_indent);
    } else if closes(kind)
        && let Some(base) = stack.pop()
        && base < *line_indent
    {
        *line_indent = base;
    }
}

fn opens(k: &TokenKind) -> bool {
    matches!(
        k,
        TokenKind::Punct(Punct::LBrace | Punct::LParen | Punct::LBracket)
            | TokenKind::TemplateHead(_)
    )
}

fn closes(k: &TokenKind) -> bool {
    matches!(
        k,
        TokenKind::Punct(Punct::RBrace | Punct::RParen | Punct::RBracket)
            | TokenKind::TemplateTail { .. }
    )
}

/// A line starting with this token continues the previous one.
fn continues_line(k: &TokenKind) -> bool {
    use Punct::*;
    matches!(
        k,
        TokenKind::Punct(
            Dot | QDot
                | AndAnd
                | OrOr
                | QQ
                | Plus
                | Star
                | Slash
                | Percent
                | EqEq
                | NotEq
                | Or
                | And
                | Caret
                | Question
        )
    )
}

/// A line ending with this token continues on the next one.
fn ends_line_open(k: &TokenKind, _text: &str) -> bool {
    use Punct::*;
    matches!(
        k,
        TokenKind::Punct(
            Eq | AndAnd
                | OrOr
                | QQ
                | Plus
                | Minus
                | Star
                | Slash
                | Percent
                | FatArrow
                | PlusEq
                | MinusEq
        )
    )
}

/// Whether the previous line's last token opened a bracket (its contents are indented by the
/// nesting already).
fn opens_on(toks: &[(TokenKind, usize, usize)], last: Option<usize>) -> bool {
    last.is_some_and(|i| opens(&toks[i].0))
}

/// `'text'` as `"text"` when that needs no new escapes (others as written).
fn double_quoted(text: &str) -> String {
    if text.len() >= 2 && text.starts_with('\'') && !text.starts_with("'''") && text.ends_with('\'')
    {
        let inner = &text[1..text.len() - 1];
        // (One character in single quotes may be a `char`: left alone.)
        let one_char = inner.chars().count() == 1 || (inner.starts_with('\\') && inner.len() <= 6);
        if !one_char && !inner.contains('"') && !inner.contains("\\'") {
            return format!("\"{inner}\"");
        }
    }
    text.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(s: &str) -> String {
        format(s, FmtOptions::default()).unwrap()
    }

    #[test]
    fn basics() {
        assert_eq!(
            f(
                "function add(a: int,b: int): int {\n    return a+b   \n}\n\n\n\nprintln( add(1 , 2) )"
            ),
            "function add(a: int, b: int): int {\n  return a+b\n}\n\nprintln(add(1, 2))\n"
        );
        assert_eq!(f("const s = 'hi'\n"), "const s = \"hi\"\n");
        assert_eq!(f("const s = 'say \"hi\"'\n"), "const s = 'say \"hi\"'\n");
    }

    #[test]
    fn comments_and_continuations() {
        let src = "// head\nconst x = foo   // trailing\n  .bar()\n  .baz()\n/* block */\nif (x) {\n// inside\nprintln(1)\n}\n";
        assert_eq!(
            f(src),
            "// head\nconst x = foo   // trailing\n  .bar()\n  .baz()\n/* block */\nif (x) {\n  // inside\n  println(1)\n}\n"
        );
    }

    #[test]
    fn idempotent() {
        let src = "const t = `a ${\n  x\n} b`\nconst y =\n  1 +\n  2\n";
        let once = f(src);
        assert_eq!(f(&once), once);
    }
}
