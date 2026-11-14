//! Style that `pico fmt` doesn't do (P10.2h): `curly`, `extraParens`, `mixedOperators`,
//! `commaDangle`, `braceStyle`, `operatorLinebreak`, `multilineTernary`, `spacedComment`,
//! `extraSemi`, `quotes`.
//!
//! - `curly` (ESLint's `multi-line`): a body without braces ends on the line of what comes
//!   before it (`if (c) f()`); one on the next line needs braces.
//! - `extraParens`: parentheses around a variable, literal, call or member read, doubled
//!   ones, and ones around a whole initializer, argument, element, field value, returned
//!   value, condition or statement. Parentheses that group operators are left alone
//!   (`mixedOperators` asks for some of them).
//! - `mixedOperators`: operators of one family (arithmetic, bitwise, comparison, logical)
//!   with different precedence, mixed without parentheses (`a + b * c`, `a && b || c`).
//! - `commaDangle` (`always-multiline`): arrays, objects, arguments, parameters, patterns
//!   and import/export lists end with a comma when the closing bracket is on a line of its
//!   own, and without one otherwise (not after a rest element).
//! - `braceStyle` (`1tbs`, one-line blocks allowed): `{` on the line of what it opens,
//!   `} else`/`} catch`/`} finally` on one line, and a multi-line block's statements on lines
//!   of their own.
//! - `operatorLinebreak` (`before`): a binary operator, an assignment's `=` or a conditional
//!   expression's `?`/`:` starts the continuation line rather than ending the line above;
//!   except `+` and `-`, which can't start a line in pico (it would begin a new statement).
//! - `multilineTernary` (`always-multiline`): a conditional expression over several lines puts
//!   its condition and both branches on lines of their own.
//! - `spacedComment`: a space after `//`, `///`, `/*` and `/**` (`/*-` is allowed).
//! - `extraSemi`: a `;` at the end of a line, or an empty statement. A `;` that ends a block's
//!   last expression changes its value, so it's kept; so is one before a line starting with
//!   `(`, `[`, a template, `+`, `-` or `/`, and the ones in a `for (…; …; …)` header.
//! - `quotes`: `'single'` (default) or `"double"` strings, unless the other kind avoids
//!   escapes; a one-character `'x'` may be a `char`, so `"double"` leaves it alone.

use crate::cx::Cx;
use crate::walk::{self, Visit};
use pico_diag::{Diagnostic, Span};
use pico_syntax::ast::*;
use pico_syntax::token::{Kw, Punct, TokenKind};
use std::collections::HashSet;

pub fn check(cx: &Cx, out: &mut Vec<Diagnostic>) {
    let mut s = Style {
        cx,
        out,
        parens: HashSet::new(),
        type_ends: HashSet::new(),
    };
    for st in &cx.module.stmts {
        s.stmt(st);
        s.paren_ctx(st);
    }
    if cx.on("spacedComment").is_some() {
        s.comments();
    }
    if cx.on("extraSemi").is_some() {
        s.semicolons();
    }
    if let Some(q) = cx.on("quotes") {
        s.quotes(q);
    }
}

struct Style<'c, 'a, 'o> {
    cx: &'c Cx<'a>,
    out: &'o mut Vec<Diagnostic>,
    /// Parentheses already reported.
    parens: HashSet<Span>,
    /// Offsets of the `}` that close type bodies.
    type_ends: HashSet<u32>,
}

/// A binary operator's family and precedence (`mixedOperators`).
fn family(op: BinOp) -> (u8, u8) {
    use BinOp::*;
    match op {
        Pow => (0, 9),
        Mul | Div | Rem => (0, 8),
        Add | Sub => (0, 7),
        Shl | Shr | UShr => (1, 6),
        BitAnd => (1, 3),
        BitXor => (1, 2),
        BitOr => (1, 1),
        Lt | Le | Gt | Ge => (2, 5),
        Eq | Ne => (2, 4),
        And => (3, 2),
        Or | Nullish => (3, 1),
    }
}

fn primary(e: &Expr) -> bool {
    matches!(
        e.kind,
        ExprKind::Ident(_)
            | ExprKind::Lit(_)
            | ExprKind::This
            | ExprKind::Member { .. }
            | ExprKind::Index { .. }
            | ExprKind::Call { .. }
            | ExprKind::Paren(_)
            | ExprKind::Array(_)
            | ExprKind::Template { .. }
    )
}

impl Style<'_, '_, '_> {
    fn warn(&mut self, rule: &str, msg: impl Into<String>, span: Span, label: &str, help: &str) {
        if self.cx.on(rule).is_some() {
            let d = self.cx.warn(rule, msg, span, label).help(help.to_string());
            self.out.push(d);
        }
    }

    fn line(&self, offset: u32) -> usize {
        self.cx.line(offset)
    }

    // --- extraParens ---

    fn extra_paren(&mut self, e: &Expr) {
        if let ExprKind::Paren(inner) = &e.kind
            && !matches!(inner.kind, ExprKind::Object { .. } | ExprKind::Block(_))
            && self.parens.insert(e.span)
        {
            self.warn(
                "extraParens",
                "these parentheses change nothing",
                e.span,
                "unnecessary",
                "remove them",
            );
        }
    }

    fn paren_ctx(&mut self, s: &Stmt) {
        if self.cx.on("extraParens").is_none() {
            return;
        }
        match &s.kind {
            StmtKind::Let { init: Some(i), .. } => self.extra_paren(i),
            StmtKind::Expr { expr, .. } => self.extra_paren(expr),
            StmtKind::While { cond, .. } | StmtKind::DoWhile { cond, .. } => self.extra_paren(cond),
            _ => {}
        }
    }

    // --- curly ---

    fn curly(&mut self, body: &Expr) {
        if matches!(body.kind, ExprKind::Block(_) | ExprKind::If { .. }) {
            return;
        }
        let Some(before) = self.cx.token_before(body.span.start) else {
            return;
        };
        if self.line(before.span.start) != self.line(body.span.end.saturating_sub(1)) {
            self.warn(
                "curly",
                "this body is on another line than its statement, so it needs braces",
                body.span,
                "no braces",
                "wrap it in `{ … }`, or put it on the statement's line",
            );
        }
    }

    // --- braceStyle ---

    fn brace_block(&mut self, b: &Block) {
        if self.cx.on("braceStyle").is_none() {
            return;
        }
        let (open, close) = (b.span.start, b.span.end.saturating_sub(1));
        if self.cx.text.as_bytes().get(open as usize) != Some(&b'{') {
            return;
        }
        if let Some(t) = self.cx.token_before(open)
            && self.line(t.span.start) != self.line(open)
            && t.kind != TokenKind::Punct(Punct::FatArrow)
        {
            self.warn(
                "braceStyle",
                "the `{` should be on the line of what it opens",
                self.cx.span(open, open + 1),
                "on its own line",
                "move it up",
            );
        }
        if self.line(open) == self.line(close) {
            return;
        }
        if let Some(first) = b.stmts.first()
            && self.line(first.span.start) == self.line(open)
        {
            self.warn(
                "braceStyle",
                "in a multi-line block, statements start on the line after `{`",
                first.span,
                "on the `{` line",
                "break the line after `{`",
            );
        }
        if let Some(last) = b.stmts.last()
            && self.line(last.span.end.saturating_sub(1)) == self.line(close)
        {
            self.warn(
                "braceStyle",
                "in a multi-line block, `}` goes on a line of its own",
                self.cx.span(close, close + 1),
                "after a statement",
                "break the line before `}`",
            );
        }
    }

    fn brace_expr(&mut self, e: &Expr) {
        if let ExprKind::Block(b) = &e.kind {
            self.brace_block(b);
        }
    }

    /// `} else`, `} catch`, `} finally` on one line.
    fn cuddled(&mut self, close_end: u32, kw: &str) {
        if self.cx.on("braceStyle").is_none() || close_end == 0 {
            return;
        }
        if self.cx.text.as_bytes().get(close_end as usize - 1) != Some(&b'}') {
            return;
        }
        let i = self.cx.token_at(close_end);
        if let Some(t) = self.cx.tokens.get(i)
            && self.line(t.span.start) != self.line(close_end - 1)
        {
            self.warn(
                "braceStyle",
                format!("`{kw}` goes on the line of the `}}` before it"),
                t.span,
                "on its own line",
                &format!("write `}} {kw}`"),
            );
        }
    }

    // --- operatorLinebreak ---

    fn linebreak(&mut self, left_end: u32, right_start: u32) {
        if self.cx.on("operatorLinebreak").is_none() {
            return;
        }
        let i = self.cx.token_at(left_end);
        let Some(op) = self.cx.tokens.get(i) else {
            return;
        };
        // A line can't start with `+` or `-` (it would begin a new statement, spec §1.2), so
        // those two stay at the end of the line.
        if op.span.start >= right_start
            || matches!(op.kind, TokenKind::Punct(Punct::Plus | Punct::Minus))
        {
            return;
        }
        let (l1, lo, l2) = (
            self.line(left_end.saturating_sub(1)),
            self.line(op.span.start),
            self.line(right_start),
        );
        if l1 != l2 && lo != l2 {
            let text = self.cx.src(op.span).to_string();
            self.warn(
                "operatorLinebreak",
                format!("`{text}` should start the next line, not end this one"),
                op.span,
                "at the end of the line",
                &format!("break the line before `{text}`"),
            );
        }
    }

    // --- commaDangle ---

    /// A list whose last element ends at `last_end` and whose closing bracket is at `close`.
    fn dangle(&mut self, last_end: u32, close: u32, rest: bool) {
        if self.cx.on("commaDangle").is_none() || rest || close <= last_end {
            return;
        }
        let comma = self
            .cx
            .find_token(last_end, close, |k| *k == TokenKind::Punct(Punct::Comma))
            .map(|t| t.span);
        let multi = self.line(last_end.saturating_sub(1)) != self.line(close);
        match (multi, comma) {
            (true, None) => self.warn(
                "commaDangle",
                "this multi-line list needs a trailing comma",
                self.cx.span(last_end.saturating_sub(1), last_end),
                "no trailing comma",
                "add `,` after the last element",
            ),
            (false, Some(c)) => self.warn(
                "commaDangle",
                "this one-line list has a trailing comma",
                c,
                "trailing comma",
                "remove it",
            ),
            _ => {}
        }
    }

    /// The offset of the first `close` bracket token after `from`.
    fn closing(&self, from: u32, p: Punct) -> Option<u32> {
        self.cx
            .find_token(from, self.cx.text.len() as u32, |k| {
                *k == TokenKind::Punct(p)
            })
            .map(|t| t.span.start)
    }

    fn pattern_dangle(&mut self, p: &Pattern) {
        let close = p.span.end.saturating_sub(1);
        match &p.kind {
            PatternKind::Struct { fields, rest, .. } => {
                if let Some(f) = fields.last() {
                    let end = f
                        .default
                        .as_ref()
                        .map(|d| d.span.end)
                        .or(f.pat.as_ref().map(|p| p.span.end))
                        .unwrap_or(f.name.span.end);
                    self.dangle(end, close, rest.is_some());
                }
            }
            PatternKind::Array(ps) => {
                if let Some(l) = ps.last() {
                    self.dangle(l.span.end, close, matches!(l.kind, PatternKind::Rest(_)));
                }
            }
            _ => {}
        }
    }

    // --- token and comment rules ---

    fn comments(&mut self) {
        for c in self.cx.comments.clone() {
            let t = self.cx.src(c);
            let rest = if let Some(r) = t.strip_prefix("///") {
                r
            } else if let Some(r) = t.strip_prefix("//") {
                r
            } else if let Some(r) = t.strip_prefix("/**") {
                if r.starts_with('/') {
                    continue; // `/**/`
                }
                r
            } else if let Some(r) = t.strip_prefix("/*") {
                if r.starts_with('-') || r.starts_with("*/") {
                    continue;
                }
                r
            } else {
                continue;
            };
            let ok =
                rest.is_empty() || rest.starts_with(char::is_whitespace) || rest.starts_with("*/");
            if !ok {
                self.warn(
                    "spacedComment",
                    "this comment needs a space after its marker",
                    c,
                    "no space",
                    "add a space after the comment marker",
                );
            }
        }
    }

    fn semicolons(&mut self) {
        let toks = &self.cx.tokens;
        // Brackets: whether each `(` opens a `for` header.
        let mut stack: Vec<bool> = Vec::new();
        let mut found = Vec::new();
        for (i, t) in toks.iter().enumerate() {
            match t.kind {
                TokenKind::Punct(Punct::LParen) => {
                    let after_for = i >= 1
                        && (toks[i - 1].kind == TokenKind::Kw(Kw::For)
                            || (i >= 2 && toks[i - 2].kind == TokenKind::Kw(Kw::For)));
                    stack.push(after_for);
                }
                TokenKind::Punct(Punct::LBracket | Punct::LBrace) => stack.push(false),
                TokenKind::Punct(Punct::RParen | Punct::RBracket | Punct::RBrace) => {
                    stack.pop();
                }
                TokenKind::Punct(Punct::Semi) if stack.last() != Some(&true) => {
                    let prev = i.checked_sub(1).map(|p| &toks[p].kind);
                    let empty = matches!(
                        prev,
                        None | Some(TokenKind::Punct(Punct::Semi | Punct::LBrace))
                    );
                    let next = toks.get(i + 1);
                    let redundant = match next {
                        None => true,
                        Some(n) if n.kind == TokenKind::Punct(Punct::RBrace) => {
                            self.type_ends.contains(&n.span.start)
                        }
                        Some(n) if self.line(n.span.start) == self.line(t.span.start) => false,
                        Some(n) => !matches!(
                            n.kind,
                            TokenKind::Punct(
                                Punct::LParen
                                    | Punct::LBracket
                                    | Punct::Plus
                                    | Punct::Minus
                                    | Punct::Slash
                            ) | TokenKind::Template(_)
                                | TokenKind::TemplateHead(_)
                                | TokenKind::Regex { .. }
                        ),
                    };
                    if empty || redundant {
                        found.push(t.span);
                    }
                }
                _ => {}
            }
        }
        for s in found {
            self.warn(
                "extraSemi",
                "this `;` changes nothing",
                s,
                "unnecessary",
                "remove it",
            );
        }
    }

    fn quotes(&mut self, setting: u32) {
        let (want, other) = if setting == 0 {
            ('\'', '"')
        } else {
            ('"', '\'')
        };
        let mut found = Vec::new();
        for t in &self.cx.tokens {
            let TokenKind::Str(value) = &t.kind else {
                continue;
            };
            let raw = self.cx.src(t.span);
            if raw.starts_with("'''") || raw.starts_with("\"\"\"") || !raw.starts_with(other) {
                continue;
            }
            if value.contains(want) || (want == '"' && value.chars().count() == 1) {
                continue;
            }
            found.push(t.span);
        }
        let name = if want == '\'' { "single" } else { "double" };
        for s in found {
            self.warn(
                "quotes",
                format!("this string should use {name} quotes"),
                s,
                "other quotes",
                &format!("write it with {want}…{want}"),
            );
        }
    }
}

impl<'a> Visit<'a> for Style<'_, '_, '_> {
    fn block(&mut self, b: &'a Block) {
        for s in &b.stmts {
            self.paren_ctx(s);
        }
        walk::walk_block(self, b);
    }

    fn decl(&mut self, d: &'a Decl) {
        match &d.kind {
            DeclKind::Struct(_)
            | DeclKind::Class(_)
            | DeclKind::Interface(_)
            | DeclKind::Enum(_) => {
                self.type_ends.insert(d.span.end.saturating_sub(1));
            }
            DeclKind::Import(ImportDecl {
                kind: ImportKind::Named(names),
                ..
            })
            | DeclKind::ExportList { names, .. } => {
                if let Some(l) = names.last() {
                    let end = l.alias.as_ref().map_or(l.name.span.end, |a| a.span.end);
                    if let Some(close) = self.closing(end, Punct::RBrace) {
                        self.dangle(end, close, false);
                    }
                }
            }
            _ => {}
        }
        walk::walk_decl(self, d);
    }

    fn func(&mut self, f: &'a FnDecl) {
        // (Only a parenthesized list: `x => …` has none; a C variadic `...` ends it.)
        let parenthesized = f.params.last().is_some_and(|p| {
            let i = self.cx.token_at(p.span.end);
            let kind = |k: usize| self.cx.tokens.get(k).map(|t| &t.kind);
            let close = TokenKind::Punct(Punct::RParen);
            kind(i) == Some(&close)
                || (kind(i) == Some(&TokenKind::Punct(Punct::Comma)) && kind(i + 1) == Some(&close))
        });
        if let (Some(l), true, false) = (f.params.last(), parenthesized, f.flags.c_variadic)
            && let Some(close) = self.closing(l.span.end, Punct::RParen)
        {
            self.dangle(l.span.end, close, l.rest);
        }
        match &f.body {
            Some(FnBody::Block(b)) => self.brace_block(b),
            Some(FnBody::Expr(e)) if self.cx.on("extraParens").is_some() => self.extra_paren(e),
            Some(FnBody::Expr(_)) | None => {}
        }
        walk::walk_fn(self, f);
    }

    fn pattern(&mut self, p: &'a Pattern) {
        self.pattern_dangle(p);
    }

    fn stmt(&mut self, s: &'a Stmt) {
        match &s.kind {
            StmtKind::While { body, .. }
            | StmtKind::For { body, .. }
            | StmtKind::ForOf { body, .. } => {
                if self.cx.on("curly").is_some() {
                    self.curly(body);
                }
                self.brace_expr(body);
            }
            StmtKind::DoWhile { body, .. } => self.brace_block(body),
            StmtKind::Let {
                pat,
                ty,
                init: Some(init),
                ..
            } => {
                let left = ty.as_ref().map_or(pat.span.end, |t| t.span.end);
                self.linebreak(left, init.span.start);
            }
            _ => {}
        }
        walk::walk_stmt(self, s);
    }

    fn expr(&mut self, e: &'a Expr) {
        let parens_on = self.cx.on("extraParens").is_some();
        match &e.kind {
            ExprKind::Paren(inner)
                if parens_on
                    && primary(inner)
                    && !matches!(
                        inner.kind,
                        ExprKind::Lit(Lit::Int { .. } | Lit::Float { .. })
                    ) =>
            {
                self.extra_paren(e)
            }
            ExprKind::If { cond, then, else_ } => {
                if parens_on {
                    self.extra_paren(cond);
                }
                if self.cx.on("curly").is_some() {
                    self.curly(then);
                    if let Some(x) = else_ {
                        self.curly(x);
                    }
                }
                self.brace_expr(then);
                if let Some(x) = else_ {
                    self.brace_expr(x);
                    self.cuddled(then.span.end, "else");
                }
            }
            ExprKind::Try {
                body,
                catches,
                finally,
            } => {
                self.brace_expr(body);
                let mut end = body.span.end;
                for c in catches {
                    self.cuddled(end, "catch");
                    self.brace_expr(&c.body);
                    end = c.body.span.end;
                }
                if let Some(f) = finally {
                    self.cuddled(end, "finally");
                    self.brace_block(f);
                }
            }
            ExprKind::Return(Some(x)) | ExprKind::Throw(x) | ExprKind::Await(x) if parens_on => {
                self.extra_paren(x)
            }
            ExprKind::Call { args, .. } | ExprKind::New { args, .. } => {
                if parens_on {
                    for a in args {
                        self.extra_paren(&a.value);
                    }
                }
                if let Some(l) = args.last() {
                    let close = e.span.end.saturating_sub(1);
                    if self.cx.text.as_bytes().get(close as usize) == Some(&b')') {
                        self.dangle(l.value.span.end, close, false);
                    }
                }
            }
            ExprKind::Array(elems) => {
                if let Some(l) = elems.last() {
                    let (ArrayElem::Expr(x) | ArrayElem::Spread(x)) = l;
                    self.dangle(x.span.end, e.span.end.saturating_sub(1), false);
                }
                if parens_on {
                    for el in elems {
                        let (ArrayElem::Expr(x) | ArrayElem::Spread(x)) = el;
                        self.extra_paren(x);
                    }
                }
            }
            ExprKind::Object { fields, .. } => {
                let end = |f: &ObjField| match f {
                    ObjField::KeyValue { value, .. }
                    | ObjField::Spread(value)
                    | ObjField::Computed { value, .. } => value.span.end,
                    ObjField::Shorthand(n) => n.span.end,
                };
                if let Some(l) = fields.last() {
                    self.dangle(end(l), e.span.end.saturating_sub(1), false);
                }
                if parens_on {
                    for f in fields {
                        if let ObjField::KeyValue { value, .. } = f {
                            self.extra_paren(value);
                        }
                    }
                }
            }
            ExprKind::Template { exprs, .. } if parens_on => {
                for x in exprs {
                    self.extra_paren(&x.expr);
                }
            }
            ExprKind::Assign { target, value, .. } => {
                if parens_on {
                    self.extra_paren(value);
                }
                self.linebreak(target.span.end, value.span.start);
            }
            ExprKind::Binary { op, lhs, rhs } => {
                self.linebreak(lhs.span.end, rhs.span.start);
                if self.cx.on("mixedOperators").is_some() {
                    let (fam, prec) = family(*op);
                    for child in [lhs, rhs] {
                        if let ExprKind::Binary { op: cop, .. } = &child.kind {
                            let (cfam, cprec) = family(*cop);
                            if cfam == fam && cop != op && cprec != prec {
                                self.warn(
                                    "mixedOperators",
                                    format!(
                                        "`{}` and `{}` are mixed without parentheses",
                                        op.as_str(),
                                        cop.as_str()
                                    ),
                                    child.span,
                                    "group it",
                                    "add parentheses to show the order",
                                );
                            }
                        }
                    }
                }
            }
            ExprKind::Ternary { cond, then, else_ } => {
                self.linebreak(cond.span.end, then.span.start);
                self.linebreak(then.span.end, else_.span.start);
                let multi = self.line(e.span.start) != self.line(e.span.end.saturating_sub(1));
                let separate = self.line(cond.span.end.saturating_sub(1))
                    != self.line(then.span.start)
                    && self.line(then.span.end.saturating_sub(1)) != self.line(else_.span.start);
                if multi && !separate {
                    self.warn(
                        "multilineTernary",
                        "this conditional expression spans several lines, so each part needs a line of its own",
                        e.span,
                        "parts share a line",
                        "put the condition, `? then` and `: else` on lines of their own",
                    );
                }
            }
            _ => {}
        }
        walk::walk_expr(self, e);
    }
}
