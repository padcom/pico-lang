//! `blankLines`: a blank line before a statement `if` (an `else if` is part of its `if`),
//! `switch` and `match`, and before a function, method, constructor, getter or setter, unless
//! it's the first in its block or type body. The comments and decorators that belong to it
//! come after the blank line. `if` and `match` in expression position (an initializer, an
//! argument) aren't statements, so they don't need one.
//!
//! The ESLint-derived constructs (decision 332): `return`, loops (`for`, `while`, `do`,
//! `loop`), `try`, block statements, classes and `export` need a blank line before them, and
//! the imports one after them; loops, `try` and blocks don't when they follow a `const`/`let`
//! or an `if`. `member`: a blank line between class members after a multi-line one.

use crate::config::Config;
use crate::walk::{self, Visit};
use pico_diag::{Diagnostic, Span, codes};
use pico_syntax::ast::*;
use pico_syntax::docs::{attached_start_line, line_of};

pub fn check(m: &Module, text: &str, cfg: &Config, out: &mut Vec<Diagnostic>) {
    let mut b = Blank { text, cfg, out };
    b.stmts(&m.stmts);
    for s in &m.stmts {
        b.stmt(s);
    }
}

struct Blank<'t, 'o> {
    text: &'t str,
    cfg: &'t Config,
    out: &'o mut Vec<Diagnostic>,
}

/// What a statement is, for the rule, and where to report it.
fn stmt_construct(s: &Stmt) -> Option<(&'static str, Span)> {
    let word = |at: Span, w: &'static str| {
        Some((w, Span::new(at.file, at.start, at.start + w.len() as u32)))
    };
    match &s.kind {
        StmtKind::Expr { expr, .. } => match &expr.kind {
            ExprKind::If { .. } => word(expr.span, "if"),
            ExprKind::Switch { .. } => word(expr.span, "switch"),
            ExprKind::Match { .. } => word(expr.span, "match"),
            ExprKind::Return(_) => word(expr.span, "return"),
            ExprKind::Loop(_) => word(expr.span, "loop"),
            ExprKind::Try { .. } => word(expr.span, "try"),
            ExprKind::Block(_) => Some((
                "block",
                Span::new(expr.span.file, expr.span.start, expr.span.start + 1),
            )),
            _ => None,
        },
        StmtKind::For { .. } | StmtKind::ForOf { .. } => word(s.span, "for"),
        StmtKind::While { .. } => word(s.span, "while"),
        StmtKind::DoWhile { .. } => word(s.span, "do"),
        StmtKind::Decl(d)
            if d.exported
                || matches!(
                    d.kind,
                    DeclKind::ExportList { .. } | DeclKind::ExportAll { .. }
                ) =>
        {
            word(d.span, "export")
        }
        StmtKind::Decl(Decl {
            kind: DeclKind::Function(f),
            span,
            ..
        }) => Some(("function", f.name.as_ref().map_or(*span, |n| n.span))),
        StmtKind::Decl(Decl {
            kind: DeclKind::Class(c),
            ..
        }) => Some(("class", c.name.span)),
        _ => None,
    }
}

fn is_import(s: &Stmt) -> bool {
    matches!(&s.kind, StmtKind::Decl(d) if matches!(d.kind, DeclKind::Import(_)))
}

/// After a `const`/`let` or an `if`, these may follow without a blank line.
fn optional_after(prev: &Stmt, construct: &str) -> bool {
    let after = match &prev.kind {
        StmtKind::Let { .. } => true,
        StmtKind::Decl(d) => matches!(d.kind, DeclKind::Binding(_)),
        StmtKind::Expr { expr, .. } => matches!(expr.kind, ExprKind::If { .. }),
        _ => false,
    };
    after && matches!(construct, "for" | "while" | "do" | "loop" | "try" | "block")
}

fn member_construct(m: &Member) -> Option<(&'static str, Span)> {
    let named = |w: &'static str, f: &FnDecl| Some((w, f.name.as_ref().map_or(m.span, |n| n.span)));
    match &m.kind {
        MemberKind::Method(f) => named("method", f),
        MemberKind::Getter(f) => named("getter", f),
        MemberKind::Setter(f) => named("setter", f),
        MemberKind::Constructor(_) => {
            let at = m.span;
            let start = at.start;
            Some((
                "constructor",
                Span::new(at.file, start, (start + 11).min(at.end)),
            ))
        }
        _ => None,
    }
}

impl Blank<'_, '_> {
    fn stmts(&mut self, stmts: &[Stmt]) {
        for w in stmts.windows(2) {
            let (prev, cur) = (&w[0], &w[1]);
            if self.cfg.blank_before("import") && is_import(prev) && !is_import(cur) {
                let at = Span::new(cur.span.file, cur.span.start, cur.span.start + 1);
                self.need(prev.span, cur.span, at, "after the imports".into());
                continue;
            }
            if let Some((construct, at)) = stmt_construct(cur)
                && self.cfg.blank_before(construct)
                && !optional_after(prev, construct)
            {
                self.need(prev.span, cur.span, at, describe(construct));
            }
        }
    }

    /// Reports a missing blank line between `prev` and `span`.
    fn need(&mut self, prev: Span, span: Span, at: Span, what: String) {
        let first = attached_start_line(self.text, span.start);
        let prev_end = line_of(self.text, prev.end.saturating_sub(1).max(prev.start));
        let lines: Vec<&str> = self.text.split('\n').collect();
        let blank =
            (prev_end + 1..first).any(|l| lines.get(l).is_some_and(|t| t.trim().is_empty()));
        if !blank {
            self.out.push(
                Diagnostic::warning(format!("a blank line is needed {what}"))
                    .with_code(codes::LINT_BLANK_LINE)
                    .primary(at, "add a blank line above")
                    .note("it's not the first in its block, so it's set apart (lint rule `blankLines`)"),
            );
        }
    }

    fn members(&mut self, ms: &[Member]) {
        for w in ms.windows(2) {
            let (prev, cur) = (&w[0], &w[1]);
            if let Some((construct, at)) = member_construct(cur)
                && self.cfg.blank_before(construct)
            {
                self.need(prev.span, cur.span, at, describe(construct));
                continue;
            }
            let multi_line = line_of(self.text, prev.span.start)
                != line_of(self.text, prev.span.end.saturating_sub(1));
            if self.cfg.blank_before("member") && multi_line {
                let at = Span::new(cur.span.file, cur.span.start, cur.span.start + 1);
                self.need(prev.span, cur.span, at, "after a multi-line member".into());
            }
        }
    }
}

fn describe(construct: &str) -> String {
    match construct {
        "function" | "method" | "constructor" | "getter" | "setter" | "class" | "block" => {
            format!("before this {construct}")
        }
        _ => format!("before this `{construct}`"),
    }
}

impl<'a> Visit<'a> for Blank<'_, '_> {
    fn block(&mut self, b: &'a Block) {
        self.stmts(&b.stmts);
        walk::walk_block(self, b);
    }

    fn decl(&mut self, d: &'a Decl) {
        match &d.kind {
            DeclKind::Struct(s) => self.members(&s.members),
            DeclKind::Class(c) => self.members(&c.members),
            DeclKind::Interface(i) => self.members(&i.members),
            DeclKind::Enum(e) => self.members(&e.members),
            _ => {}
        }
        walk::walk_decl(self, d);
    }
}
