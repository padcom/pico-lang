//! `complexity`: a function's cyclomatic complexity. It starts at 1 and adds 1 for each `if`
//! and `else if`, loop, `case` and `match` arm, `catch`, conditional expression, `&&`, `||`
//! and `??`. A closure is a function of its own: it adds nothing to the function around it.

use crate::walk::{self, Visit};
use pico_diag::{Diagnostic, Span, codes};
use pico_syntax::ast::*;

pub fn check(m: &Module, text: &str, max: u32, out: &mut Vec<Diagnostic>) {
    let mut c = Functions { text, max, out };
    for s in &m.stmts {
        c.stmt(s);
    }
}

/// Finds the functions (declared, methods, closures) and reports the complex ones.
struct Functions<'t, 'o> {
    text: &'t str,
    max: u32,
    out: &'o mut Vec<Diagnostic>,
}

impl<'a> Visit<'a> for Functions<'_, '_> {
    fn member(&mut self, m: &'a Member) {
        if let MemberKind::Constructor(f) = &m.kind {
            self.report(f, "the constructor".into(), first_line(self.text, m.span));
            // (Its body's closures; the constructor itself is done.)
            walk::walk_fn(self, f);
            return;
        }
        walk::walk_member(self, m);
    }

    fn func(&mut self, f: &'a FnDecl) {
        let (what, span) = match &f.name {
            Some(n) => (format!("`{}`", n.name), n.span),
            None => ("this function".into(), first_line(self.text, f.span)),
        };
        self.report(f, what, span);
        walk::walk_fn(self, f);
    }
}

impl Functions<'_, '_> {
    fn report(&mut self, f: &FnDecl, what: String, at: Span) {
        let n = complexity(f);
        if n > self.max {
            self.out.push(
                Diagnostic::warning(format!(
                    "{what} has a cyclomatic complexity of {n} (the maximum is {})",
                    self.max
                ))
                .with_code(codes::LINT_COMPLEXITY)
                .primary(at, format!("complexity {n}"))
                .help("split it into smaller functions"),
            );
        }
    }
}

/// The cyclomatic complexity of `f`'s body.
pub fn complexity(f: &FnDecl) -> u32 {
    let mut c = Counter { n: 1, root: true };
    c.func(f);
    c.n
}

struct Counter {
    n: u32,
    /// The function being measured hasn't been entered yet.
    root: bool,
}

impl<'a> Visit<'a> for Counter {
    fn func(&mut self, f: &'a FnDecl) {
        // Nested functions and closures are measured on their own.
        if std::mem::replace(&mut self.root, false) {
            walk::walk_fn(self, f);
        }
    }

    fn stmt(&mut self, s: &'a Stmt) {
        if matches!(
            s.kind,
            StmtKind::While { .. }
                | StmtKind::DoWhile { .. }
                | StmtKind::For { .. }
                | StmtKind::ForOf { .. }
        ) {
            self.n += 1;
        }
        walk::walk_stmt(self, s);
    }

    fn expr(&mut self, e: &'a Expr) {
        self.n += match &e.kind {
            ExprKind::If { .. } | ExprKind::Ternary { .. } | ExprKind::Loop(_) => 1,
            ExprKind::Binary { op, .. } => {
                matches!(op, BinOp::And | BinOp::Or | BinOp::Nullish) as u32
            }
            // Each case but `default`, each arm but a final catch-all.
            ExprKind::Switch { cases, .. } => {
                cases.iter().filter(|c| c.pat.is_some()).count() as u32
            }
            ExprKind::Match { arms, .. } => {
                let catch_all = arms.last().is_some_and(|a| {
                    a.guard.is_none() && matches!(a.pat.kind, PatternKind::Wildcard)
                });
                (arms.len() - catch_all as usize) as u32
            }
            ExprKind::Try { catches, .. } => catches.len() as u32,
            _ => 0,
        };
        walk::walk_expr(self, e);
    }
}

/// The part of `span` on its first line (a closure's start, for the squiggle).
pub fn first_line(text: &str, span: Span) -> Span {
    let s = &text[span.start as usize..span.end as usize];
    let len = s.find('\n').unwrap_or(s.len());
    Span::new(span.file, span.start, span.start + len as u32)
}
