//! `blankLines`: a blank line before a statement `if` (an `else if` is part of its `if`),
//! `switch` and `match`, and before a function, method, constructor, getter or setter, unless
//! it's the first in its block or type body. The comments and decorators that belong to it
//! come after the blank line. `if` and `match` in expression position (an initializer, an
//! argument) aren't statements, so they don't need one.

use crate::config::Config;
use crate::walk::{self, Visit};
use pico_diag::{Diagnostic, Span, codes};
use pico_syntax::ast::*;
use pico_syntax::docs::{attached_start_line, line_of};

pub fn check(m: &Module, text: &str, cfg: &Config, out: &mut Vec<Diagnostic>) {
    let mut b = Blank { text, cfg, out };
    b.items(
        m.stmts
            .iter()
            .map(|s| (s.span, stmt_construct(s)))
            .collect(),
    );
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
            _ => None,
        },
        StmtKind::Decl(Decl {
            kind: DeclKind::Function(f),
            span,
            ..
        }) => Some(("function", f.name.as_ref().map_or(*span, |n| n.span))),
        _ => None,
    }
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
    /// One block's or type body's items: (the item's span, what it is).
    fn items(&mut self, items: Vec<(Span, Option<(&'static str, Span)>)>) {
        for w in items.windows(2) {
            let ((prev, _), (span, what)) = (w[0], w[1]);
            let Some((construct, at)) = what else {
                continue;
            };
            if !self.cfg.blank_before(construct) {
                continue;
            }
            let first = attached_start_line(self.text, span.start);
            let prev_end = line_of(self.text, prev.end.saturating_sub(1).max(prev.start));
            let lines: Vec<&str> = self.text.split('\n').collect();
            let blank =
                (prev_end + 1..first).any(|l| lines.get(l).is_some_and(|t| t.trim().is_empty()));
            if !blank {
                self.out.push(
                    Diagnostic::warning(if matches!(construct, "if" | "switch" | "match") {
                        format!("a blank line is needed before this `{construct}`")
                    } else {
                        format!("a blank line is needed before this {construct}")
                    })
                        .with_code(codes::LINT_BLANK_LINE)
                        .primary(at, "add a blank line above")
                        .note("it's not the first in its block, so it's set apart (lint rule `blankLines`)"),
                );
            }
        }
    }

    fn members(&mut self, ms: &[Member]) {
        self.items(ms.iter().map(|m| (m.span, member_construct(m))).collect());
    }
}

impl<'a> Visit<'a> for Blank<'_, '_> {
    fn block(&mut self, b: &'a Block) {
        self.items(
            b.stmts
                .iter()
                .map(|s| (s.span, stmt_construct(s)))
                .collect(),
        );
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
