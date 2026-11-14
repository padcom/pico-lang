//! Size limits (P10.2d): `maxParams`, `maxDepth`, `maxLines`, `maxNestedCallbacks`,
//! `maxStatementsPerLine`, `maxLen`.
//!
//! - `maxParams`: parameters of a function, method, constructor or closure (not of `extern`
//!   declarations, whose shape C decides).
//! - `maxDepth`: `if` (an `else if` is part of its `if`), loops, `switch`, `match` and `try`
//!   nested inside each other in one function; a closure starts again at 0.
//! - `maxLines`: a function's lines, from its first to its last, blank lines and comments
//!   included.
//! - `maxNestedCallbacks`: functions passed as arguments, nested inside each other.
//! - `maxStatementsPerLine`: statements starting on one line (nested ones included).
//! - `maxLen`: characters on a line.

use crate::complexity::first_line;
use crate::cx::Cx;
use crate::walk::{self, Visit};
use pico_diag::{Diagnostic, Span};
use pico_syntax::ast::*;
use std::collections::BTreeMap;

pub fn check(cx: &Cx, out: &mut Vec<Diagnostic>) {
    let mut s = Sizes {
        cx,
        out,
        depth: vec![0],
        callbacks: 0,
        lines: BTreeMap::new(),
    };
    for st in &cx.module.stmts {
        s.stmt(st);
    }
    if let Some(max) = cx.on("maxStatementsPerLine") {
        for spans in s.lines.values() {
            if let Some(extra) = spans.get(max as usize) {
                let msg = format!(
                    "this line has {} statements (the maximum is {max})",
                    spans.len()
                );
                let d = cx
                    .warn(
                        "maxStatementsPerLine",
                        msg,
                        *extra,
                        "one statement too many",
                    )
                    .help("put each statement on a line of its own");
                s.out.push(d);
            }
        }
    }
    if let Some(max) = cx.on("maxLen") {
        for l in 0..cx.line_count() {
            let line = cx.line_text(l);
            let n = line.chars().count();
            if n > max as usize {
                let cut = line.char_indices().nth(max as usize).map_or(0, |(i, _)| i);
                let start = cx.line_start(l) + cut as u32;
                let span = cx.span(start, cx.line_start(l) + line.len() as u32);
                let msg = format!("this line is {n} characters long (the maximum is {max})");
                out.push(
                    cx.warn("maxLen", msg, span, "past the maximum")
                        .help("break the line"),
                );
            }
        }
    }
}

struct Sizes<'c, 'a, 'o> {
    cx: &'c Cx<'a>,
    out: &'o mut Vec<Diagnostic>,
    /// The nesting depth in each function being visited (innermost last).
    depth: Vec<u32>,
    callbacks: u32,
    /// Statement starts per line.
    lines: BTreeMap<usize, Vec<Span>>,
}

impl Sizes<'_, '_, '_> {
    fn name(&self, f: &FnDecl) -> (String, Span) {
        match &f.name {
            Some(n) => (format!("`{}`", n.name), n.span),
            None => ("this function".into(), first_line(self.cx.text, f.span)),
        }
    }

    /// Enters a nesting construct starting at `at` (its keyword, `len` bytes).
    fn nest(&mut self, at: Span, len: u32, inner: impl FnOnce(&mut Self)) {
        let d = self.depth.last_mut().expect("a function");
        *d += 1;
        let depth = *d;
        if let Some(max) = self.cx.on("maxDepth")
            && depth == max + 1
        {
            let span = self.cx.span(at.start, at.start + len);
            let msg = format!("blocks are nested {depth} deep (the maximum is {max})");
            self.out.push(
                self.cx
                    .warn("maxDepth", msg, span, "too deep")
                    .help("move the inner part into a function of its own, or return early"),
            );
        }
        inner(self);
        *self.depth.last_mut().expect("a function") -= 1;
    }

    /// The body of an `if` chain: an `else if` stays at its `if`'s depth.
    fn if_chain(&mut self, e: &Expr) {
        if let ExprKind::If { cond, then, else_ } = &e.kind {
            self.expr(cond);
            self.expr(then);
            if let Some(x) = else_ {
                if matches!(x.kind, ExprKind::If { .. }) {
                    self.if_chain(x);
                } else {
                    self.expr(x);
                }
            }
        }
    }
}

impl<'a> Visit<'a> for Sizes<'_, '_, '_> {
    fn decl(&mut self, d: &'a Decl) {
        if matches!(d.kind, DeclKind::Extern { .. }) {
            return;
        }
        walk::walk_decl(self, d);
    }

    fn func(&mut self, f: &'a FnDecl) {
        let (what, at) = self.name(f);
        if let Some(max) = self.cx.on("maxParams") {
            let n = f.params.len();
            if n > max as usize {
                let msg = format!("{what} has {n} parameters (the maximum is {max})");
                self.out.push(
                    self.cx
                        .warn("maxParams", msg, at, "too many parameters")
                        .help("group related parameters in a struct"),
                );
            }
        }
        if let (Some(max), Some(_)) = (self.cx.on("maxLines"), &f.body) {
            let n = self.cx.line(f.span.end.saturating_sub(1)) - self.cx.line(f.span.start) + 1;
            if n > max as usize {
                let msg = format!("{what} has {n} lines (the maximum is {max})");
                self.out.push(
                    self.cx
                        .warn("maxLines", msg, at, "too long")
                        .help("split it into smaller functions"),
                );
            }
        }
        self.depth.push(0);
        walk::walk_fn(self, f);
        self.depth.pop();
    }

    fn stmt(&mut self, s: &'a Stmt) {
        self.lines
            .entry(self.cx.line(s.span.start))
            .or_default()
            .push(s.span);
        match &s.kind {
            StmtKind::While { .. }
            | StmtKind::DoWhile { .. }
            | StmtKind::For { .. }
            | StmtKind::ForOf { .. } => {
                let len = if matches!(s.kind, StmtKind::DoWhile { .. }) {
                    2
                } else {
                    3
                };
                let len = if matches!(s.kind, StmtKind::While { .. }) {
                    5
                } else {
                    len
                };
                self.nest(s.span, len, |me| walk::walk_stmt(me, s));
            }
            _ => walk::walk_stmt(self, s),
        }
    }

    fn expr(&mut self, e: &'a Expr) {
        match &e.kind {
            ExprKind::If { .. } => self.nest(e.span, 2, |me| me.if_chain(e)),
            ExprKind::Loop(_) => self.nest(e.span, 4, |me| walk::walk_expr(me, e)),
            ExprKind::Switch { .. } => self.nest(e.span, 6, |me| walk::walk_expr(me, e)),
            ExprKind::Match { .. } => self.nest(e.span, 5, |me| walk::walk_expr(me, e)),
            ExprKind::Try { .. } => self.nest(e.span, 3, |me| walk::walk_expr(me, e)),
            ExprKind::Call { callee, args, .. } | ExprKind::New { callee, args, .. } => {
                self.expr(callee);
                for a in args {
                    let ExprKind::Function(f) = &a.value.kind else {
                        self.expr(&a.value);
                        continue;
                    };
                    self.callbacks += 1;
                    if let Some(max) = self.cx.on("maxNestedCallbacks")
                        && self.callbacks == max + 1
                    {
                        let at = first_line(self.cx.text, a.value.span);
                        let msg = format!(
                            "callbacks are nested {} deep (the maximum is {max})",
                            self.callbacks
                        );
                        self.out.push(
                                self.cx
                                    .warn("maxNestedCallbacks", msg, at, "too deep")
                                    .help("give the inner callback a name and declare it outside, or use `await`"),
                            );
                    }
                    self.func(f);
                    self.callbacks -= 1;
                }
            }
            _ => walk::walk_expr(self, e),
        }
    }
}
