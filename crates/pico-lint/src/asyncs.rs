//! Async (P10.2f): `floatingPromise`, `awaitInLoop`, `requireAwait`, `preferAwait`,
//! `promiseReturnWrap`, `promiseParamNames`.
//!
//! - `floatingPromise`: a statement whose value is a `Promise` that can fail (its error type
//!   isn't `never`) and is dropped (not the block's value), unless it ends in `.catch(…)` or
//!   `.then(onValue, onError)`.
//! - `awaitInLoop`: `await` in a loop's body or condition (a `for await` loop's body is meant
//!   to wait), not counting nested functions.
//! - `requireAwait`: an `async` function (not a generator) with a body but no `await` and no
//!   `for await`.
//! - `preferAwait`: `.then`/`.catch`/`.finally` on a promise, a `callback`/`cb` parameter, and
//!   a callback argument whose first parameter is `err`/`error`.
//! - `promiseReturnWrap`: a `then`/`catch`/`finally` callback whose result is
//!   `Promise.resolve(x)` or `Promise.reject(e)`.
//! - `promiseParamNames`: `new Promise((resolve, reject) => …)`'s parameters named otherwise.

use crate::cx::Cx;
use crate::scope::strip_parens;
use crate::walk::{self, Visit};
use pico_diag::{Diagnostic, Span};
use pico_sema::ty::Ty;
use pico_syntax::ast::*;

pub fn check(cx: &Cx, out: &mut Vec<Diagnostic>) {
    let rules = [
        "floatingPromise",
        "awaitInLoop",
        "requireAwait",
        "preferAwait",
        "promiseReturnWrap",
        "promiseParamNames",
    ];
    if !rules.iter().any(|r| cx.on(r).is_some()) {
        return;
    }
    let mut a = Async {
        cx,
        out,
        loops: vec![0],
    };
    a.dropped(&cx.module.stmts, true);
    for s in &cx.module.stmts {
        a.stmt(s);
    }
}

struct Async<'c, 'a, 'o> {
    cx: &'c Cx<'a>,
    out: &'o mut Vec<Diagnostic>,
    /// Loops around the current point, per function.
    loops: Vec<u32>,
}

fn is_promise(t: &Ty) -> bool {
    matches!(t, Ty::Class(_, name, _) if &**name == "Promise")
}

/// A promise that can fail: its error type isn't `never` (an `async` function without
/// `throws` returns `Promise<T, never>`, decision 234).
fn can_fail(t: &Ty) -> bool {
    match t {
        Ty::Class(_, name, args) if &**name == "Promise" => !matches!(args.get(1), Some(Ty::Never)),
        _ => false,
    }
}

/// `x.then(…)`, `x.catch(…)`, `x.finally(…)`: the method's name and the arguments.
fn promise_method(e: &Expr) -> Option<(&Ident, &Expr, &[Arg])> {
    let ExprKind::Call { callee, args, .. } = &e.kind else {
        return None;
    };
    let ExprKind::Member { object, prop, .. } = &callee.kind else {
        return None;
    };
    matches!(prop.name.as_str(), "then" | "catch" | "finally").then_some((
        prop,
        &**object,
        &args[..],
    ))
}

/// The expressions a function's result comes from: `return x` and its final expression.
fn results<'e>(f: &'e FnDecl, out: &mut Vec<&'e Expr>) {
    struct R<'e, 'o> {
        out: &'o mut Vec<&'e Expr>,
        root: bool,
    }
    impl<'e> Visit<'e> for R<'e, '_> {
        fn func(&mut self, f: &'e FnDecl) {
            if std::mem::replace(&mut self.root, false) {
                walk::walk_fn(self, f);
            }
        }
        fn expr(&mut self, e: &'e Expr) {
            if let ExprKind::Return(Some(x)) = &e.kind {
                self.out.push(x);
            }
            walk::walk_expr(self, e);
        }
    }
    match &f.body {
        Some(FnBody::Expr(e)) => out.push(e),
        Some(FnBody::Block(b)) => {
            if let Some(Stmt {
                kind: StmtKind::Expr { expr, semi: false },
                ..
            }) = b.stmts.last()
            {
                out.push(expr);
            }
        }
        None => {}
    }
    R { out, root: true }.func(f);
}

/// Whether `f` awaits (an `await` or a `for await`), not looking into nested functions.
fn awaits(f: &FnDecl) -> bool {
    struct A {
        found: bool,
        root: bool,
    }
    impl<'a> Visit<'a> for A {
        fn func(&mut self, f: &'a FnDecl) {
            if std::mem::replace(&mut self.root, false) {
                walk::walk_fn(self, f);
            }
        }
        fn stmt(&mut self, s: &'a Stmt) {
            if matches!(
                s.kind,
                StmtKind::ForOf { is_await: true, .. } | StmtKind::Using { is_await: true, .. }
            ) {
                self.found = true;
            }
            walk::walk_stmt(self, s);
        }
        fn expr(&mut self, e: &'a Expr) {
            if matches!(e.kind, ExprKind::Await(_)) {
                self.found = true;
            }
            walk::walk_expr(self, e);
        }
    }
    let mut a = A {
        found: false,
        root: true,
    };
    a.func(f);
    a.found
}

fn param_name(p: &Param) -> Option<&str> {
    match &p.pat.kind {
        PatternKind::Ident(n) => Some(&n.name),
        _ => None,
    }
}

impl Async<'_, '_, '_> {
    fn dropped(&mut self, stmts: &[Stmt], all: bool) {
        if self.cx.on("floatingPromise").is_none() {
            return;
        }
        for (i, s) in stmts.iter().enumerate() {
            let StmtKind::Expr { expr, semi } = &s.kind else {
                continue;
            };
            if !(all || *semi || i + 1 < stmts.len()) {
                continue;
            }
            let e = strip_parens(expr);
            if !matches!(e.kind, ExprKind::Call { .. } | ExprKind::New { .. }) {
                continue;
            }
            if !self.cx.ty(e.span).is_some_and(can_fail) {
                continue;
            }
            let handled = match promise_method(e) {
                Some((m, _, args)) => m.name == "catch" || (m.name == "then" && args.len() >= 2),
                None => false,
            };
            if !handled {
                self.out.push(
                    self.cx
                        .warn(
                            "floatingPromise",
                            "this promise is dropped: its failure would go unnoticed",
                            e.span,
                            "not awaited",
                        )
                        .help("`await` it, return it, or end it with `.catch(…)`"),
                );
            }
        }
    }

    fn warn(&mut self, rule: &str, msg: &str, span: Span, label: &str, help: &str) {
        if self.cx.on(rule).is_some() {
            let d = self.cx.warn(rule, msg, span, label).help(help.to_string());
            self.out.push(d);
        }
    }

    fn in_loop(&mut self, f: impl FnOnce(&mut Self)) {
        *self.loops.last_mut().expect("a function") += 1;
        f(self);
        *self.loops.last_mut().expect("a function") -= 1;
    }
}

impl<'a> Visit<'a> for Async<'_, '_, '_> {
    fn block(&mut self, b: &'a Block) {
        self.dropped(&b.stmts, false);
        walk::walk_block(self, b);
    }

    fn func(&mut self, f: &'a FnDecl) {
        if f.flags.is_async && !f.flags.generator && self.cx.on("requireAwait").is_some() {
            let empty = matches!(&f.body, Some(FnBody::Block(b)) if b.stmts.is_empty());
            if f.body.is_some() && !empty && !awaits(f) {
                let at = match &f.name {
                    Some(n) => n.span,
                    None => crate::complexity::first_line(self.cx.text, f.span),
                };
                self.warn(
                    "requireAwait",
                    "this `async` function never awaits",
                    at,
                    "no `await`",
                    "remove `async`, or await what it waits for",
                );
            }
        }
        if f.name.is_some() {
            for p in &f.params {
                if let Some(n @ ("callback" | "cb")) = param_name(p) {
                    let msg = format!("`{n}` takes a callback where returning a promise would do");
                    self.warn(
                        "preferAwait",
                        &msg,
                        p.pat.span,
                        "a callback",
                        "return a `Promise` and let callers `await` it",
                    );
                }
            }
        }
        self.loops.push(0);
        walk::walk_fn(self, f);
        self.loops.pop();
    }

    fn stmt(&mut self, s: &'a Stmt) {
        match &s.kind {
            StmtKind::While { .. } | StmtKind::DoWhile { .. } | StmtKind::For { .. } => {
                self.in_loop(|me| walk::walk_stmt(me, s));
            }
            StmtKind::ForOf {
                is_await,
                pat,
                iter,
                body,
                ..
            } => {
                self.pattern(pat);
                self.expr(iter);
                if *is_await {
                    let saved = std::mem::replace(self.loops.last_mut().expect("a function"), 0);
                    self.expr(body);
                    *self.loops.last_mut().expect("a function") = saved;
                } else {
                    self.in_loop(|me| me.expr(body));
                }
            }
            _ => walk::walk_stmt(self, s),
        }
    }

    fn expr(&mut self, e: &'a Expr) {
        match &e.kind {
            ExprKind::Loop(_) => return self.in_loop(|me| walk::walk_expr(me, e)),
            ExprKind::Await(_) if *self.loops.last().expect("a function") > 0 => {
                self.warn(
                    "awaitInLoop",
                    "this `await` makes each iteration wait for the previous one",
                    e.span,
                    "in a loop",
                    "start the work first and await `Promise.all(…)` after the loop, if the iterations don't depend on each other",
                );
            }
            ExprKind::New { callee, args, .. } => {
                if matches!(&callee.kind, ExprKind::Ident(n) if n == "Promise")
                    && let Some(ExprKind::Function(f)) = args.first().map(|a| &a.value.kind)
                {
                    let want = ["resolve", "reject"];
                    for (p, w) in f.params.iter().zip(want) {
                        let ok = param_name(p).is_some_and(|n| n.trim_start_matches('_') == w);
                        if !ok {
                            let msg = format!("the executor's parameter should be named `{w}`");
                            self.warn(
                                "promiseParamNames",
                                &msg,
                                p.pat.span,
                                "misnamed",
                                &format!("call it `{w}`"),
                            );
                        }
                    }
                }
            }
            ExprKind::Call { args, .. } => {
                if let Some((m, object, args)) = promise_method(e) {
                    let typed = self.cx.ty(strip_parens(object).span);
                    if typed.is_none_or(is_promise) {
                        let msg = format!("`.{}(…)` where `await` would do", m.name);
                        self.warn("preferAwait", &msg, m.span, "promise chain", "await the promise in an `async` function (with `try`/`catch`/`finally` for the other two)");
                        for a in args {
                            if let ExprKind::Function(f) = &a.value.kind {
                                let mut rs = Vec::new();
                                results(f, &mut rs);
                                for r in rs {
                                    let r = strip_parens(r);
                                    if let ExprKind::Call { callee, .. } = &r.kind
                                        && let ExprKind::Member { object, prop, .. } = &callee.kind
                                    {
                                        let wrap = matches!(&object.kind, ExprKind::Ident(n) if n == "Promise")
                                            && matches!(prop.name.as_str(), "resolve" | "reject");
                                        if wrap {
                                            let msg = format!(
                                                "`Promise.{}(…)` in a promise callback",
                                                prop.name
                                            );
                                            let help = if prop.name == "resolve" {
                                                "return the value itself"
                                            } else {
                                                "throw the error instead"
                                            };
                                            self.warn(
                                                "promiseReturnWrap",
                                                &msg,
                                                r.span,
                                                "wrapped",
                                                help,
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    }
                } else {
                    for a in args {
                        if let ExprKind::Function(f) = &a.value.kind
                            && let Some(n @ ("err" | "error")) =
                                f.params.first().and_then(param_name)
                        {
                            let msg =
                                format!("a callback taking `{n}` first, where a promise would do");
                            let at = f.params[0].pat.span;
                            self.warn(
                                "preferAwait",
                                &msg,
                                at,
                                "error-first callback",
                                "use an API that returns a `Promise` and `await` it",
                            );
                        }
                    }
                }
            }
            _ => {}
        }
        walk::walk_expr(self, e);
    }
}
