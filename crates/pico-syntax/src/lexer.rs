//! The lexer: source text to tokens (spec §1.1, §1.3–§1.6).
//!
//! Newlines are kept as [`TokenKind::Newline`] tokens; [`crate::newline`] decides later which of
//! them end statements. Templates are split into head/middle/tail tokens around their
//! substitutions, using a stack of open substitutions. A regex literal can start only where an
//! expression can start, which the lexer tracks from the previous significant token.

use crate::token::{Kw, NumSuffix, Punct, Token, TokenKind};
use pico_diag::{Diagnostic, FileId, Span, codes};

/// Lexes a whole file. The token list always ends with [`TokenKind::Eof`].
pub fn lex(file: FileId, src: &str) -> (Vec<Token>, Vec<Diagnostic>) {
    let mut lx = Lexer {
        src,
        pos: 0,
        file,
        tokens: Vec::new(),
        diags: Vec::new(),
        expr_end: false,
        subs: Vec::new(),
    };
    lx.run();
    (lx.tokens, lx.diags)
}

/// Placeholder for a substitution while dedenting a multiline template.
const HOLE: char = '\u{E000}';

struct TemplatePart {
    token: usize,
    raw: String,
}

struct Template {
    triple: bool,
    start: usize,
    parts: Vec<TemplatePart>,
}

/// An open `${ … }` substitution.
struct Sub {
    braces: u32,
    nest: u32,
    ternary: u32,
    spec: Option<String>,
    template: Template,
}

struct Lexer<'a> {
    src: &'a str,
    pos: usize,
    file: FileId,
    tokens: Vec<Token>,
    diags: Vec<Diagnostic>,
    /// Whether the previous significant token can end an expression (so `/` means division).
    expr_end: bool,
    subs: Vec<Sub>,
}

impl<'a> Lexer<'a> {
    fn span(&self, start: usize, end: usize) -> Span {
        Span::new(self.file, start as u32, end as u32)
    }

    fn rest(&self) -> &'a str {
        &self.src[self.pos..]
    }

    fn peek(&self) -> Option<char> {
        self.rest().chars().next()
    }

    fn peek_at(&self, n: usize) -> Option<char> {
        self.rest().chars().nth(n)
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += c.len_utf8();
        Some(c)
    }

    fn push(&mut self, kind: TokenKind, start: usize) {
        self.expr_end = match &kind {
            TokenKind::Ident(_)
            | TokenKind::Int { .. }
            | TokenKind::Float { .. }
            | TokenKind::Str(_)
            | TokenKind::Template(_)
            | TokenKind::TemplateTail { .. }
            | TokenKind::Regex { .. } => true,
            // (After `operator`, `/` is the operator being declared, not a regex.)
            TokenKind::Kw(k) => matches!(
                k,
                Kw::This | Kw::Super | Kw::Null | Kw::True | Kw::False | Kw::Operator
            ),
            TokenKind::Punct(p) => match p {
                Punct::RParen | Punct::RBracket => true,
                // Postfix `!`, `++` and `--` keep the state of the operand before them.
                Punct::Bang | Punct::PlusPlus | Punct::MinusMinus => self.expr_end,
                _ => false,
            },
            TokenKind::Newline => self.expr_end,
            TokenKind::TemplateHead(_) | TokenKind::TemplateMiddle { .. } | TokenKind::Eof => false,
        };
        let span = self.span(start, self.pos);
        let newline_before = matches!(self.tokens.last(), Some(t) if t.kind == TokenKind::Newline);
        self.tokens.push(Token {
            kind,
            span,
            newline_before,
        });
    }

    fn error(&mut self, code: pico_diag::Code, msg: &str, start: usize, end: usize, label: &str) {
        let span = self.span(start, end);
        self.diags
            .push(Diagnostic::error(code, msg).primary(span, label));
    }

    fn run(&mut self) {
        if self.rest().starts_with("#!") {
            while let Some(c) = self.peek() {
                if c == '\n' {
                    break;
                }
                self.bump();
            }
        }
        loop {
            self.skip_whitespace();
            let start = self.pos;
            let Some(c) = self.peek() else {
                if let Some(sub) = self.subs.last() {
                    let s = sub.template.start;
                    self.error(
                        codes::UNTERMINATED_TEMPLATE,
                        "unterminated template literal",
                        s,
                        s + 1,
                        "template starts here",
                    );
                    self.subs.clear();
                }
                self.push(TokenKind::Eof, start);
                return;
            };
            match c {
                '\n' => {
                    self.bump();
                    if !matches!(
                        self.tokens.last().map(|t| &t.kind),
                        Some(TokenKind::Newline)
                    ) && !self.tokens.is_empty()
                    {
                        self.push(TokenKind::Newline, start);
                    }
                }
                '/' if self.peek_at(1) == Some('/') => self.line_comment(),
                '/' if self.peek_at(1) == Some('*') => self.block_comment(),
                '`' => {
                    let triple = self.rest().starts_with("```");
                    self.pos += if triple { 3 } else { 1 };
                    let template = Template {
                        triple,
                        start,
                        parts: Vec::new(),
                    };
                    self.template_text(start, template, None);
                }
                '"' | '\'' => self.string(c),
                '0'..='9' => self.number(),
                '.' if self.peek_at(1).is_some_and(|c| c.is_ascii_digit()) => self.number(),
                c if is_ident_start(c) => self.ident(),
                '/' if !self.expr_end => self.regex(),
                '}' if self.subs.last().is_some_and(|s| s.braces == 0) => {
                    let sub = self.subs.pop().unwrap();
                    self.bump();
                    self.template_text(start, sub.template, Some(sub.spec));
                }
                ':' if self
                    .subs
                    .last()
                    .is_some_and(|s| s.braces == 0 && s.nest == 0 && s.ternary == 0) =>
                {
                    self.format_spec()
                }
                _ => self.punct(),
            }
        }
    }

    fn skip_whitespace(&mut self) {
        while let Some(c) = self.peek() {
            if c == '\n' || !(c.is_whitespace() || c == '\u{FEFF}') {
                break;
            }
            self.bump();
        }
    }

    fn line_comment(&mut self) {
        while let Some(c) = self.peek() {
            if c == '\n' {
                break;
            }
            self.bump();
        }
    }

    fn block_comment(&mut self) {
        let start = self.pos;
        self.pos += 2;
        match self.rest().find("*/") {
            Some(i) => {
                let had_newline = self.rest()[..i].contains('\n');
                self.pos += i + 2;
                // A comment spanning lines separates statements like a line break does.
                if had_newline && !self.tokens.is_empty() {
                    self.push(TokenKind::Newline, start);
                }
            }
            None => {
                self.pos = self.src.len();
                self.error(
                    codes::UNTERMINATED_COMMENT,
                    "unterminated block comment",
                    start,
                    start + 2,
                    "comment starts here",
                );
            }
        }
    }

    fn ident(&mut self) {
        let start = self.pos;
        self.bump();
        while self.peek().is_some_and(is_ident_continue) {
            self.bump();
        }
        let text = &self.src[start..self.pos];
        let kind = match Kw::parse(text) {
            Some(k) => TokenKind::Kw(k),
            None => TokenKind::Ident(text.to_string()),
        };
        self.push(kind, start);
    }

    fn punct(&mut self) {
        let start = self.pos;
        let rest = self.rest();
        let found = Punct::ALL.iter().copied().find(|p| {
            rest.starts_with(p.as_str())
                // `a?.5:b` is a ternary, not optional chaining.
                && !(*p == Punct::QDot && rest[2..].starts_with(|c: char| c.is_ascii_digit()))
        });
        match found {
            Some(p) => {
                self.pos += p.as_str().len();
                if let Some(sub) = self.subs.last_mut() {
                    match p {
                        Punct::LBrace => sub.braces += 1,
                        Punct::RBrace => sub.braces = sub.braces.saturating_sub(1),
                        Punct::LParen | Punct::LBracket => sub.nest += 1,
                        Punct::RParen | Punct::RBracket => sub.nest = sub.nest.saturating_sub(1),
                        Punct::Question if sub.braces == 0 && sub.nest == 0 => sub.ternary += 1,
                        Punct::Colon if sub.braces == 0 && sub.nest == 0 => {
                            sub.ternary = sub.ternary.saturating_sub(1)
                        }
                        _ => {}
                    }
                }
                self.push(TokenKind::Punct(p), start);
            }
            None => {
                let c = self.bump().unwrap();
                self.error(
                    codes::UNEXPECTED_CHAR,
                    &format!("unexpected character `{}`", c.escape_debug()),
                    start,
                    self.pos,
                    "not valid here",
                );
            }
        }
    }

    // ---- numbers ----

    fn digits(&mut self, radix: u32) -> String {
        let mut s = String::new();
        while let Some(c) = self.peek() {
            if c == '_' {
                self.bump();
            } else if c.is_digit(radix) {
                s.push(c);
                self.bump();
            } else {
                break;
            }
        }
        s
    }

    fn number(&mut self) {
        let start = self.pos;
        let radix = match (self.peek(), self.peek_at(1)) {
            (Some('0'), Some('x' | 'X')) => 16,
            (Some('0'), Some('o' | 'O')) => 8,
            (Some('0'), Some('b' | 'B')) => 2,
            _ => 10,
        };
        let mut is_float = false;
        let mut text;
        if radix != 10 {
            self.pos += 2;
            text = self.digits(radix);
            if text.is_empty() {
                self.error(
                    codes::INVALID_NUMBER,
                    "missing digits after the radix prefix",
                    start,
                    self.pos,
                    "expected digits",
                );
            }
        } else {
            text = self.digits(10);
            if self.peek() == Some('.') && self.peek_at(1).is_some_and(|c| c.is_ascii_digit()) {
                self.bump();
                text.push('.');
                text.push_str(&self.digits(10));
                is_float = true;
            }
            if matches!(self.peek(), Some('e' | 'E')) {
                let sign = matches!(self.peek_at(1), Some('+' | '-'));
                let digit_at = if sign { 2 } else { 1 };
                if self.peek_at(digit_at).is_some_and(|c| c.is_ascii_digit()) {
                    self.bump();
                    text.push('e');
                    if sign {
                        text.push(self.bump().unwrap());
                    }
                    text.push_str(&self.digits(10));
                    is_float = true;
                }
            }
        }
        // Suffix: any identifier characters glued to the literal.
        let suffix_start = self.pos;
        while self.peek().is_some_and(is_ident_continue) {
            self.bump();
        }
        let suffix_text = &self.src[suffix_start..self.pos];
        let mut suffix = None;
        if !suffix_text.is_empty() {
            match NumSuffix::parse(suffix_text) {
                Some(s) if s.is_float() && radix != 10 => self.error(
                    codes::INVALID_NUMBER,
                    "float suffix on a non-decimal literal",
                    suffix_start,
                    self.pos,
                    "not allowed here",
                ),
                Some(s) if !s.is_float() && is_float => self.error(
                    codes::INVALID_NUMBER,
                    &format!("integer suffix `{suffix_text}` on a float literal"),
                    suffix_start,
                    self.pos,
                    "use `f32` or `f64`",
                ),
                Some(s) => suffix = Some(s),
                None => {
                    let d = Diagnostic::error(
                        codes::INVALID_NUMBER,
                        format!("invalid suffix `{suffix_text}` on a numeric literal"),
                    )
                    .primary(self.span(suffix_start, self.pos), "unknown suffix")
                    .help("valid suffixes: i8 i16 i32 i64 u8 u16 u32 u64 isize usize f32 f64");
                    self.diags.push(d);
                }
            }
        }
        let kind = if is_float || suffix.is_some_and(|s| s.is_float()) {
            TokenKind::Float {
                value: text.parse().unwrap_or(0.0),
                suffix,
            }
        } else {
            let value = match u64::from_str_radix(&text, radix) {
                Ok(v) => v,
                Err(_) if text.is_empty() => 0,
                Err(_) => {
                    self.error(
                        codes::NUMBER_TOO_LARGE,
                        "integer literal is too large",
                        start,
                        suffix_start,
                        "doesn't fit in 64 bits",
                    );
                    0
                }
            };
            TokenKind::Int { value, suffix }
        };
        self.push(kind, start);
    }

    // ---- strings ----

    fn string(&mut self, quote: char) {
        let start = self.pos;
        let triple: String = std::iter::repeat_n(quote, 3).collect();
        if self.rest().starts_with(&triple) {
            self.pos += 3;
            let body_start = self.pos;
            let mut end = None;
            while let Some(c) = self.peek() {
                if c == '\\' {
                    self.bump();
                    self.bump();
                } else if self.rest().starts_with(&triple) {
                    end = Some(self.pos);
                    self.pos += 3;
                    break;
                } else {
                    self.bump();
                }
            }
            let Some(end) = end else {
                self.error(
                    codes::UNTERMINATED_STRING,
                    "unterminated multiline string literal",
                    start,
                    start + 3,
                    "string starts here",
                );
                let raw = dedent(&self.src[body_start..]);
                let text = self.cook(&raw, body_start);
                self.push(TokenKind::Str(text), start);
                return;
            };
            let raw = dedent(&self.src[body_start..end]);
            let text = self.cook(&raw, body_start);
            self.push(TokenKind::Str(text), start);
            return;
        }
        self.bump();
        let body_start = self.pos;
        loop {
            match self.peek() {
                None | Some('\n') => {
                    let line_end = self.pos;
                    self.error(
                        codes::UNTERMINATED_STRING,
                        "unterminated string literal",
                        start,
                        line_end,
                        "string starts here",
                    );
                    let text = self.cook(&self.src[body_start..line_end], body_start);
                    self.push(TokenKind::Str(text), start);
                    return;
                }
                Some('\\') => {
                    self.bump();
                    self.bump();
                }
                Some(c) if c == quote => {
                    let end = self.pos;
                    self.bump();
                    let text = self.cook(&self.src[body_start..end], body_start);
                    self.push(TokenKind::Str(text), start);
                    return;
                }
                Some(_) => {
                    self.bump();
                }
            }
        }
    }

    /// Processes escapes. `base` is the byte offset of `raw` in the source, for error spans
    /// (approximate for dedented text).
    fn cook(&mut self, raw: &str, base: usize) -> String {
        let mut out = String::with_capacity(raw.len());
        let mut it = raw.char_indices().peekable();
        while let Some((i, c)) = it.next() {
            if c != '\\' {
                out.push(c);
                continue;
            }
            let Some((_, e)) = it.next() else {
                break;
            };
            let simple = match e {
                'n' => Some('\n'),
                'r' => Some('\r'),
                't' => Some('\t'),
                '0' => Some('\0'),
                'b' => Some('\u{8}'),
                'f' => Some('\u{c}'),
                'v' => Some('\u{b}'),
                '\\' | '\'' | '"' | '`' | '$' => Some(e),
                _ => None,
            };
            if let Some(ch) = simple {
                out.push(ch);
                continue;
            }
            if e == '\n' {
                continue; // line continuation
            }
            let bad = |lx: &mut Lexer, len: usize, msg: &str| {
                let s = (base + i).min(lx.src.len());
                lx.error(
                    codes::INVALID_ESCAPE,
                    msg,
                    s,
                    (s + len).min(lx.src.len()),
                    "invalid escape",
                );
            };
            match e {
                'x' => {
                    let h: String = (0..2).filter_map(|_| it.next().map(|(_, c)| c)).collect();
                    match u8::from_str_radix(&h, 16) {
                        Ok(v) if h.len() == 2 && v <= 0x7F => out.push(v as char),
                        _ => bad(self, 2 + h.len(), "`\\x` takes two hex digits up to 7F"),
                    }
                }
                'u' => {
                    let code = if it.peek().is_some_and(|(_, c)| *c == '{') {
                        it.next();
                        let mut h = String::new();
                        let mut closed = false;
                        for (_, c) in it.by_ref() {
                            if c == '}' {
                                closed = true;
                                break;
                            }
                            h.push(c);
                        }
                        if closed && (1..=6).contains(&h.len()) {
                            u32::from_str_radix(&h, 16).ok()
                        } else {
                            None
                        }
                    } else {
                        let h: String = (0..4).filter_map(|_| it.next().map(|(_, c)| c)).collect();
                        if h.len() == 4 {
                            u32::from_str_radix(&h, 16).ok()
                        } else {
                            None
                        }
                    };
                    match code.and_then(char::from_u32) {
                        Some(ch) => out.push(ch),
                        None => bad(self, 2, "invalid unicode escape"),
                    }
                }
                _ => bad(
                    self,
                    1 + e.len_utf8(),
                    &format!("unknown escape sequence `\\{}`", e.escape_debug()),
                ),
            }
        }
        out
    }

    // ---- templates ----

    /// Lexes template text starting at `self.pos` (just after `` ` `` or `}`) until `${` or the
    /// closing delimiter. `spec` is `Some` when continuing after a substitution.
    fn template_text(
        &mut self,
        start: usize,
        mut template: Template,
        spec: Option<Option<String>>,
    ) {
        let text_start = self.pos;
        let closing = if template.triple { "```" } else { "`" };
        let mut ended = None; // Some(true) = closed, Some(false) = substitution
        while let Some(c) = self.peek() {
            if c == '\\' {
                self.bump();
                self.bump();
            } else if self.rest().starts_with(closing) {
                ended = Some(true);
                break;
            } else if self.rest().starts_with("${") {
                ended = Some(false);
                break;
            } else {
                self.bump();
            }
        }
        let raw = self.src[text_start..self.pos].to_string();
        let first = spec.is_none();
        let spec = spec.flatten();
        let kind = match (ended, first) {
            (Some(false), true) => {
                self.pos += 2;
                TokenKind::TemplateHead(String::new())
            }
            (Some(false), false) => {
                self.pos += 2;
                TokenKind::TemplateMiddle {
                    spec,
                    text: String::new(),
                }
            }
            (closed, true) => {
                if closed.is_some() {
                    self.pos += closing.len();
                }
                TokenKind::Template(String::new())
            }
            (closed, false) => {
                if closed.is_some() {
                    self.pos += closing.len();
                }
                TokenKind::TemplateTail {
                    spec,
                    text: String::new(),
                }
            }
        };
        if ended.is_none() {
            let s = template.start;
            self.error(
                codes::UNTERMINATED_TEMPLATE,
                "unterminated template literal",
                s,
                s + closing.len(),
                "template starts here",
            );
        }
        template.parts.push(TemplatePart {
            token: self.tokens.len(),
            raw,
        });
        self.push(kind, start);
        if ended == Some(false) {
            self.subs.push(Sub {
                braces: 0,
                nest: 0,
                ternary: 0,
                spec: None,
                template,
            });
        } else {
            self.finish_template(template, text_start);
        }
    }

    /// Dedents (for ```` ``` ````) and cooks every text part of a finished template.
    fn finish_template(&mut self, template: Template, base: usize) {
        let mut raws: Vec<String> = template.parts.iter().map(|p| p.raw.clone()).collect();
        if template.triple {
            let joined = raws.join(&HOLE.to_string());
            let split: Vec<String> = dedent(&joined).split(HOLE).map(str::to_string).collect();
            if split.len() == raws.len() {
                raws = split;
            }
        }
        for (part, raw) in template.parts.iter().zip(raws) {
            let text = self.cook(&raw, base);
            match &mut self.tokens[part.token].kind {
                TokenKind::Template(t)
                | TokenKind::TemplateHead(t)
                | TokenKind::TemplateMiddle { text: t, .. }
                | TokenKind::TemplateTail { text: t, .. } => *t = text,
                _ => unreachable!("template part index points at a non-template token"),
            }
        }
    }

    /// `:` at the top level of a substitution: everything up to the closing `}` is a format spec.
    fn format_spec(&mut self) {
        self.bump();
        let start = self.pos;
        while let Some(c) = self.peek() {
            if c == '}' || c == '\n' {
                break;
            }
            self.bump();
        }
        let spec = self.src[start..self.pos].to_string();
        if let Some(sub) = self.subs.last_mut() {
            sub.spec = Some(spec);
        }
    }

    // ---- regex ----

    fn regex(&mut self) {
        let start = self.pos;
        self.bump();
        let body_start = self.pos;
        let mut in_class = false;
        loop {
            match self.peek() {
                None | Some('\n') => {
                    self.error(
                        codes::UNTERMINATED_REGEX,
                        "unterminated regex literal",
                        start,
                        self.pos,
                        "regex starts here",
                    );
                    let pattern = self.src[body_start..self.pos].to_string();
                    self.push(
                        TokenKind::Regex {
                            pattern,
                            flags: String::new(),
                        },
                        start,
                    );
                    return;
                }
                Some('\\') => {
                    self.bump();
                    self.bump();
                }
                Some('[') => {
                    in_class = true;
                    self.bump();
                }
                Some(']') => {
                    in_class = false;
                    self.bump();
                }
                Some('/') if !in_class => break,
                Some(_) => {
                    self.bump();
                }
            }
        }
        let pattern = self.src[body_start..self.pos].to_string();
        self.bump();
        let flags_start = self.pos;
        while self.peek().is_some_and(is_ident_continue) {
            self.bump();
        }
        let flags = self.src[flags_start..self.pos].to_string();
        self.push(TokenKind::Regex { pattern, flags }, start);
    }
}

fn is_ident_start(c: char) -> bool {
    c == '$' || c == '_' || unicode_ident::is_xid_start(c)
}

fn is_ident_continue(c: char) -> bool {
    c == '$' || c == '\u{200C}' || c == '\u{200D}' || unicode_ident::is_xid_continue(c)
}

/// Multiline-literal indentation stripping (spec §1.3): drop the first line break when the
/// opening line is blank, drop a blank last line, then remove the leading whitespace common to
/// every non-blank line.
pub fn dedent(raw: &str) -> String {
    let is_blank = |s: &str| s.chars().all(|c| c == ' ' || c == '\t' || c == '\r');
    let mut s = raw;
    if let Some(i) = s.find('\n')
        && is_blank(&s[..i])
    {
        s = &s[i + 1..];
    }
    match s.rfind('\n') {
        Some(i) if is_blank(&s[i + 1..]) => s = &s[..i],
        None if is_blank(s) => s = "",
        _ => {}
    }
    let lines: Vec<&str> = s.split('\n').collect();
    let mut common: Option<&str> = None;
    for line in &lines {
        if is_blank(line) {
            continue;
        }
        let ws_len = line.len() - line.trim_start_matches([' ', '\t']).len();
        let ws = &line[..ws_len];
        common = Some(match common {
            None => ws,
            Some(c) => {
                let n = c
                    .bytes()
                    .zip(ws.bytes())
                    .take_while(|(a, b)| a == b)
                    .count();
                &c[..n]
            }
        });
    }
    let n = common.map_or(0, str::len);
    lines
        .iter()
        .map(|l| if is_blank(l) { "" } else { &l[n..] })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests;
