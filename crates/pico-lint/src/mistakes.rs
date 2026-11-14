//! Likely mistakes (P10.2e): `shadow`, `unusedExpressions`, `unreachable`, `selfCompare`,
//! `selfAssign`, `constantBinary`, `unsafeFinally`, `requireYield`, `forDirection`,
//! `emptyBlock`, `dupeElseIf`, `uselessCatch`, `templateInString`, `loopFunc`; and
//! `underscoreDangle` and `preferNamedCaptureGroup` (P10.2i).

use crate::cx::Cx;
use crate::scope::{self, Kind, strip_parens};
use crate::walk::{self, Visit};
use pico_diag::{Diagnostic, Span};
use pico_sema::ty::Ty;
use pico_syntax::ast::*;
use std::collections::HashSet;

pub fn check(cx: &Cx, out: &mut Vec<Diagnostic>) {
    let scoped = ["shadow", "loopFunc", "underscoreDangle"];
    if scoped.iter().any(|r| cx.on(r).is_some()) {
        scopes(cx, out);
    }
    let mut m = Mistakes {
        cx,
        out,
        finally: None,
        reactive: reactive_names(cx.module),
        tracking: 0,
    };
    if cx.on("unusedExpressions").is_some() {
        m.dropped(&cx.module.stmts, true);
    }
    m.unreachable(&cx.module.stmts);
    for s in &cx.module.stmts {
        m.stmt(s);
    }
}

fn kind_name(k: Kind) -> &'static str {
    match k {
        Kind::Const => "constant",
        Kind::Let | Kind::ForLet => "variable",
        Kind::Param => "parameter",
        Kind::Function => "function",
        Kind::Type => "type",
        Kind::Import => "import",
        Kind::Pattern => "binding",
        Kind::Member => "member",
    }
}

fn scopes(cx: &Cx, out: &mut Vec<Diagnostic>) {
    let s = scope::resolve(cx.module);
    if cx.on("shadow").is_some() {
        for &(inner, outer) in &s.shadows {
            let (i, o) = (&s.bindings[inner], &s.bindings[outer]);
            let msg = format!(
                "`{}` hides the {} `{}` declared on line {}",
                i.name,
                kind_name(o.kind),
                o.name,
                cx.line(o.span.start) + 1
            );
            out.push(
                cx.warn("shadow", msg, i.span, "hides an outer declaration")
                    .help("give it a name of its own"),
            );
        }
    }
    if cx.on("loopFunc").is_some() {
        let mut seen = HashSet::new();
        for r in &s.refs {
            let Some(lc) = r.closure else { continue };
            let b = &s.bindings[r.binding];
            if r.binding < lc.before && b.kind == Kind::Let && b.writes > 0 && seen.insert(lc.span)
            {
                let at = crate::complexity::first_line(cx.text, lc.span);
                let msg = format!(
                    "this closure captures `{}`, which changes while the loop runs",
                    b.name
                );
                out.push(
                    cx.warn("loopFunc", msg, at, "created in a loop")
                        .help("copy the value into a `const` inside the loop and capture that"),
                );
            }
        }
    }
    if cx.on("underscoreDangle").is_some() {
        for b in &s.bindings {
            // (`__name`s are the core library's internals, decision 333.)
            let dangling = b.name.len() > 1
                && !b.name.starts_with("__")
                && !b.name.chars().all(|c| c == '_')
                && (b.name.starts_with('_') || b.name.ends_with('_'));
            let kinds = [
                Kind::Const,
                Kind::Let,
                Kind::ForLet,
                Kind::Param,
                Kind::Function,
                Kind::Pattern,
            ];
            if dangling && b.reads > 0 && kinds.contains(&b.kind) {
                let msg = format!(
                    "`{}` is used, but a leading or trailing `_` marks a name as unused",
                    b.name
                );
                out.push(
                    cx.warn("underscoreDangle", msg, b.span, "used")
                        .help("drop the `_`"),
                );
            }
        }
    }
}

struct Mistakes<'c, 'a, 'o> {
    cx: &'c Cx<'a>,
    out: &'o mut Vec<Diagnostic>,
    /// Inside a `finally` block: the loops entered since.
    finally: Option<u32>,
    /// Names of reactive bindings: reading one in an effect subscribes it (spec §16).
    reactive: HashSet<String>,
    /// Inside a callback of `watchEffect`/`computed`, where reads subscribe.
    tracking: u32,
}

/// The names of the module's reactive bindings (`reactive x = …`, `reactive` fields).
fn reactive_names(m: &Module) -> HashSet<String> {
    struct R {
        names: HashSet<String>,
    }
    impl<'a> Visit<'a> for R {
        fn stmt(&mut self, s: &'a Stmt) {
            if let StmtKind::Let {
                kind: BindingKind::Reactive,
                pat,
                ..
            } = &s.kind
            {
                let mut ns = Vec::new();
                walk::bound_names(pat, &mut ns);
                self.names.extend(ns.into_iter().map(|n| n.name.clone()));
            }
            walk::walk_stmt(self, s);
        }
        fn member(&mut self, m: &'a Member) {
            if let (true, MemberKind::Field { name, .. }) = (m.mods.reactive, &m.kind) {
                self.names.insert(name.name.clone());
            }
            walk::walk_member(self, m);
        }
    }
    let mut r = R {
        names: HashSet::new(),
    };
    for s in &m.stmts {
        r.stmt(s);
    }
    r.names
}

/// Whether `e` does something besides computing a value: a call, assignment, `++`, `await`,
/// `yield` or `throw` anywhere in it (not counting closures).
fn effects(e: &Expr) -> bool {
    struct F {
        found: bool,
    }
    impl<'a> Visit<'a> for F {
        fn func(&mut self, _: &'a FnDecl) {}
        fn expr(&mut self, e: &'a Expr) {
            if matches!(
                e.kind,
                ExprKind::Call { .. }
                    | ExprKind::New { .. }
                    | ExprKind::Assign { .. }
                    | ExprKind::Await(_)
                    | ExprKind::Yield { .. }
                    | ExprKind::Throw(_)
                    | ExprKind::Unary {
                        op: UnOp::PreInc | UnOp::PreDec | UnOp::PostInc | UnOp::PostDec,
                        ..
                    }
            ) {
                self.found = true;
            }
            walk::walk_expr(self, e);
        }
    }
    let mut f = F { found: false };
    f.expr(e);
    f.found
}

/// The variable a read starts from (`a` in `a.b[c]`).
fn root(e: &Expr) -> Option<&str> {
    match &strip_parens(e).kind {
        ExprKind::Ident(n) => Some(n),
        ExprKind::Member { object, .. }
        | ExprKind::Index { object, .. }
        | ExprKind::NonNull(object) => root(object),
        _ => None,
    }
}

/// An expression whose value is all it does (dropping it does nothing): its description.
fn pure(e: &Expr) -> Option<&'static str> {
    if effects(e) {
        return None;
    }
    Some(match &e.kind {
        ExprKind::Lit(_) => "a literal",
        ExprKind::Ident(_) | ExprKind::This => "a variable",
        ExprKind::Template { tag: None, .. } => "a template",
        ExprKind::Array(_) | ExprKind::ArrayRepeat { .. } => "an array",
        ExprKind::Object { .. } => "an object",
        ExprKind::Member { .. } | ExprKind::Index { .. } => "a member read",
        ExprKind::Binary { .. } => "an operation",
        ExprKind::Unary {
            op: UnOp::Neg | UnOp::Plus | UnOp::Not | UnOp::BitNot | UnOp::Typeof,
            ..
        } => "an operation",
        ExprKind::Ternary { .. } => "a conditional expression",
        ExprKind::Function(_) => "a function",
        ExprKind::Range { .. } => "a range",
        ExprKind::As { .. } | ExprKind::Is { .. } | ExprKind::Instanceof { .. } => "a test",
        ExprKind::Paren(x) | ExprKind::NonNull(x) => return pure(x),
        _ => return None,
    })
}

/// A plain place: a variable, `this`, or a chain of member reads and indexes of one.
fn simple(e: &Expr) -> bool {
    match &strip_parens(e).kind {
        ExprKind::Ident(_) | ExprKind::This => true,
        ExprKind::Member { object, .. } => simple(object),
        ExprKind::Index { object, index } => {
            simple(object)
                && matches!(
                    strip_parens(index).kind,
                    ExprKind::Lit(_) | ExprKind::Ident(_)
                )
        }
        _ => false,
    }
}

fn norm(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

/// Whether a type can't hold `null` (`None`: not known for certain).
fn non_null(t: &Ty) -> bool {
    matches!(
        t,
        Ty::Int(_)
            | Ty::Float(_)
            | Ty::Bool
            | Ty::Char
            | Ty::Str
            | Ty::Struct(..)
            | Ty::Class(..)
            | Ty::Array(_)
            | Ty::Fixed(..)
            | Ty::Tuple(_)
            | Ty::Enum(..)
            | Ty::Range(_)
            | Ty::Lits(..)
    )
}

fn diverges(s: &Stmt) -> bool {
    matches!(
        &s.kind,
        StmtKind::Expr { expr, .. } if matches!(
            expr.kind,
            ExprKind::Return(_) | ExprKind::Throw(_) | ExprKind::Break(_) | ExprKind::Continue
        )
    )
}

/// Whether `e` contains a node `pred` accepts, not looking into nested functions.
fn contains(f: &FnDecl, pred: fn(&Expr) -> bool) -> bool {
    struct Find {
        pred: fn(&Expr) -> bool,
        found: bool,
        root: bool,
    }
    impl<'a> Visit<'a> for Find {
        fn func(&mut self, f: &'a FnDecl) {
            if std::mem::replace(&mut self.root, false) {
                walk::walk_fn(self, f);
            }
        }
        fn expr(&mut self, e: &'a Expr) {
            if (self.pred)(e) {
                self.found = true;
            }
            walk::walk_expr(self, e);
        }
    }
    let mut v = Find {
        pred,
        found: false,
        root: true,
    };
    v.func(f);
    v.found
}

impl Mistakes<'_, '_, '_> {
    /// `unusedExpressions`: statements whose value is dropped (all but a block's last, which
    /// is its value, unless it ends in `;`).
    fn dropped(&mut self, stmts: &[Stmt], all: bool) {
        for (i, s) in stmts.iter().enumerate() {
            let StmtKind::Expr { expr, semi } = &s.kind else {
                continue;
            };
            if !(all || *semi || i + 1 < stmts.len()) {
                continue;
            }
            if let ExprKind::New { .. } = strip_parens(expr).kind {
                self.out.push(
                    self.cx
                        .warn(
                            "unusedExpressions",
                            "this object is created and dropped",
                            expr.span,
                            "never used",
                        )
                        .help("store it, or call a function for the effect instead"),
                );
            } else if let Some(what) = pure(expr) {
                let subscribes = root(expr).is_some_and(|r| self.reactive.contains(r))
                    || (self.tracking > 0 && root(expr).is_some());
                if subscribes {
                    continue;
                }
                let msg = format!("{what} whose value is dropped does nothing");
                self.out.push(
                    self.cx
                        .warn("unusedExpressions", msg, expr.span, "no effect")
                        .help("remove it, or use its value"),
                );
            }
        }
    }

    fn unreachable(&mut self, stmts: &[Stmt]) {
        if self.cx.on("unreachable").is_none() {
            return;
        }
        let Some(i) = stmts.iter().position(diverges) else {
            return;
        };
        let rest: Vec<&Stmt> = stmts[i + 1..]
            .iter()
            .filter(|s| !matches!(&s.kind, StmtKind::Decl(d) if matches!(d.kind, DeclKind::Function(_))))
            .collect();
        if let (Some(first), Some(last)) = (rest.first(), rest.last()) {
            let span = self.cx.span(first.span.start, last.span.end);
            self.out.push(
                self.cx
                    .warn("unreachable", "this code never runs", span, "unreachable")
                    .help("remove it, or move it before the statement that leaves"),
            );
        }
    }

    fn empty(&mut self, b: &Block, span: Span) {
        if self.cx.on("emptyBlock").is_none() || !b.stmts.is_empty() {
            return;
        }
        if self.cx.has_comment(span.start, span.end) {
            return;
        }
        self.out.push(
            self.cx
                .warn("emptyBlock", "this block is empty", span, "empty")
                .help("remove it, or put a comment inside saying why it's empty"),
        );
    }

    fn if_chain(&mut self, e: &Expr) {
        if self.cx.on("dupeElseIf").is_none() {
            return;
        }
        let mut seen: Vec<String> = Vec::new();
        let mut cur = e;
        while let ExprKind::If { cond, else_, .. } = &cur.kind {
            let c = norm(self.cx.src(strip_parens(cond).span));
            if seen.contains(&c) {
                self.out.push(
                    self.cx
                        .warn(
                            "dupeElseIf",
                            "this condition was already tested earlier in the chain",
                            cond.span,
                            "never true here",
                        )
                        .help("remove the branch, or fix the condition"),
                );
            }
            seen.push(c);
            let mut ors = Vec::new();
            or_operands(strip_parens(cond), &mut ors);
            if ors.len() > 1 {
                seen.extend(ors.iter().map(|o| norm(self.cx.src(o.span))));
            }
            match else_ {
                Some(x) => cur = x,
                None => break,
            }
        }
    }

    fn for_direction(&mut self, s: &Stmt) {
        let StmtKind::For {
            cond: Some(cond),
            update: Some(update),
            ..
        } = &s.kind
        else {
            return;
        };
        let ExprKind::Binary { op, lhs, rhs } = &strip_parens(cond).kind else {
            return;
        };
        let (counter, up_wanted) = match (op, &lhs.kind, &rhs.kind) {
            (BinOp::Lt | BinOp::Le, ExprKind::Ident(n), _) => (n, true),
            (BinOp::Gt | BinOp::Ge, ExprKind::Ident(n), _) => (n, false),
            (BinOp::Lt | BinOp::Le, _, ExprKind::Ident(n)) => (n, false),
            (BinOp::Gt | BinOp::Ge, _, ExprKind::Ident(n)) => (n, true),
            _ => return,
        };
        let up = match &strip_parens(update).kind {
            ExprKind::Unary { op, expr } if matches!(&expr.kind, ExprKind::Ident(n) if n == counter) => {
                match op {
                    UnOp::PreInc | UnOp::PostInc => true,
                    UnOp::PreDec | UnOp::PostDec => false,
                    _ => return,
                }
            }
            ExprKind::Assign {
                op: Some(op @ (BinOp::Add | BinOp::Sub)),
                target,
                value,
            } if matches!(&target.kind, ExprKind::Ident(n) if n == counter) => {
                let positive = match &strip_parens(value).kind {
                    ExprKind::Lit(Lit::Int { .. } | Lit::Float { .. }) => true,
                    ExprKind::Unary {
                        op: UnOp::Neg,
                        expr,
                    } if matches!(expr.kind, ExprKind::Lit(_)) => false,
                    _ => return,
                };
                (*op == BinOp::Add) == positive
            }
            _ => return,
        };
        if up != up_wanted {
            self.out.push(
                self.cx
                    .warn(
                        "forDirection",
                        format!("the update moves `{counter}` away from the loop's end"),
                        update.span,
                        "wrong direction",
                    )
                    .help("count the other way, or fix the condition"),
            );
        }
    }

    fn useless_catch(&mut self, catches: &[Catch]) {
        let Some(c) = catches.last() else { return };
        let PatternKind::Ident(n) = &c.pat.kind else {
            return;
        };
        let body = match &c.body.kind {
            ExprKind::Block(b) if b.stmts.len() == 1 => match &b.stmts[0].kind {
                StmtKind::Expr { expr, .. } => expr,
                _ => return,
            },
            _ => &c.body,
        };
        if let ExprKind::Throw(x) = &body.kind
            && matches!(&strip_parens(x).kind, ExprKind::Ident(m) if *m == n.name)
        {
            self.out.push(
                self.cx
                    .warn(
                        "uselessCatch",
                        "this `catch` only rethrows what it caught",
                        crate::complexity::first_line(self.cx.text, c.span),
                        "does nothing",
                    )
                    .help("remove it: the error passes through without it"),
            );
        }
    }

    fn regex(&mut self, pattern: &str, span: Span) {
        let b = pattern.as_bytes();
        let (mut i, mut class) = (0, false);
        while i < b.len() {
            match b[i] {
                b'\\' => i += 1,
                b'[' => class = true,
                b']' => class = false,
                b'(' if !class && b.get(i + 1) != Some(&b'?') => {
                    self.out.push(
                        self.cx
                            .warn(
                                "preferNamedCaptureGroup",
                                "this regex has an unnamed capture group",
                                span,
                                "unnamed group",
                            )
                            .help("name it, `(?<name>…)`, or don't capture, `(?:…)`"),
                    );
                    return;
                }
                _ => {}
            }
            i += 1;
        }
    }
}

/// Whether a member chain has a `?.` in it.
fn optional_chain(e: &Expr) -> bool {
    match &strip_parens(e).kind {
        ExprKind::Member {
            object, optional, ..
        } => *optional || optional_chain(object),
        ExprKind::Index { object, .. } | ExprKind::NonNull(object) => optional_chain(object),
        ExprKind::Call { callee, .. } => optional_chain(callee),
        _ => false,
    }
}

fn or_operands<'e>(e: &'e Expr, out: &mut Vec<&'e Expr>) {
    match &e.kind {
        ExprKind::Binary {
            op: BinOp::Or,
            lhs,
            rhs,
        } => {
            or_operands(strip_parens(lhs), out);
            or_operands(strip_parens(rhs), out);
        }
        _ => out.push(e),
    }
}

impl<'a> Visit<'a> for Mistakes<'_, '_, '_> {
    fn block(&mut self, b: &'a Block) {
        if self.cx.on("unusedExpressions").is_some() {
            self.dropped(&b.stmts, false);
        }
        self.unreachable(&b.stmts);
        walk::walk_block(self, b);
    }

    fn func(&mut self, f: &'a FnDecl) {
        if self.cx.on("requireYield").is_some() && f.flags.generator {
            let empty = matches!(&f.body, Some(FnBody::Block(b)) if b.stmts.is_empty());
            if !empty
                && f.body.is_some()
                && !contains(f, |e| matches!(e.kind, ExprKind::Yield { .. }))
            {
                let at = match &f.name {
                    Some(n) => n.span,
                    None => crate::complexity::first_line(self.cx.text, f.span),
                };
                self.out.push(
                    self.cx
                        .warn(
                            "requireYield",
                            "this generator never yields",
                            at,
                            "no `yield`",
                        )
                        .help("yield its values, or make it an ordinary function"),
                );
            }
        }
        // A `: void` function's final expression is dropped too.
        let void = f
            .ret
            .as_ref()
            .is_some_and(|t| self.cx.src(t.span) == "void");
        if let (true, Some(FnBody::Block(b)), Some(_)) =
            (void, &f.body, self.cx.on("unusedExpressions"))
            && let Some(last) = b.stmts.last()
        {
            self.dropped(std::slice::from_ref(last), true);
        }
        let saved = self.finally.take();
        walk::walk_fn(self, f);
        self.finally = saved;
    }

    fn stmt(&mut self, s: &'a Stmt) {
        if self.cx.on("forDirection").is_some() {
            self.for_direction(s);
        }
        let is_loop = matches!(
            s.kind,
            StmtKind::While { .. }
                | StmtKind::DoWhile { .. }
                | StmtKind::For { .. }
                | StmtKind::ForOf { .. }
        );
        if let StmtKind::DoWhile { body, .. } = &s.kind {
            self.empty(body, s.span);
        }
        if is_loop && let Some(n) = self.finally.as_mut() {
            *n += 1;
        }
        walk::walk_stmt(self, s);
        if is_loop && let Some(n) = self.finally.as_mut() {
            *n -= 1;
        }
    }

    fn expr(&mut self, e: &'a Expr) {
        match &e.kind {
            ExprKind::Block(b) | ExprKind::Unsafe(b) | ExprKind::Arena(b) => self.empty(b, e.span),
            ExprKind::Loop(b) => self.empty(b, e.span),
            ExprKind::Switch { cases, .. } if cases.is_empty() => {
                if self.cx.on("emptyBlock").is_some() {
                    self.out.push(
                        self.cx
                            .warn("emptyBlock", "this `switch` has no cases", e.span, "empty")
                            .help("remove it"),
                    );
                }
            }
            ExprKind::If { .. } => self.if_chain_once(e),
            ExprKind::Call { callee, .. } if matches!(&callee.kind, ExprKind::Ident(n) if n == "watchEffect" || n == "computed") =>
            {
                self.tracking += 1;
                walk::walk_expr(self, e);
                self.tracking -= 1;
                return;
            }
            ExprKind::Binary { op, lhs, rhs } => {
                let cmp = matches!(
                    op,
                    BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge
                );
                // `x != x` on a float is pico's test for NaN (there's no `isNaN`).
                let float = self
                    .cx
                    .ty(strip_parens(lhs).span)
                    .is_some_and(|t| matches!(t, Ty::Float(_)));
                if cmp
                    && self.cx.on("selfCompare").is_some()
                    && !float
                    && simple(lhs)
                    && norm(self.cx.src(strip_parens(lhs).span))
                        == norm(self.cx.src(strip_parens(rhs).span))
                {
                    self.out.push(
                        self.cx
                            .warn(
                                "selfCompare",
                                "this compares a value with itself",
                                e.span,
                                "always the same result",
                            )
                            .help("compare it with something else"),
                    );
                }
                if *op == BinOp::Nullish && self.cx.on("constantBinary").is_some() {
                    self.non_null_left(lhs, e.span, "??");
                }
            }
            ExprKind::Assign { op, target, value } => {
                if op.is_none()
                    && self.cx.on("selfAssign").is_some()
                    && simple(target)
                    && norm(self.cx.src(strip_parens(target).span))
                        == norm(self.cx.src(strip_parens(value).span))
                {
                    self.out.push(
                        self.cx
                            .warn(
                                "selfAssign",
                                "this assigns a value to itself",
                                e.span,
                                "changes nothing",
                            )
                            .help("remove it, or assign what you meant"),
                    );
                }
                if *op == Some(BinOp::Nullish) && self.cx.on("constantBinary").is_some() {
                    self.non_null_left(target, e.span, "??=");
                }
            }
            ExprKind::Try {
                catches, finally, ..
            } => {
                if self.cx.on("uselessCatch").is_some() {
                    self.useless_catch(catches);
                }
                if self.cx.on("emptyBlock").is_some() {
                    for c in catches {
                        if let ExprKind::Block(b) = &c.body.kind
                            && b.stmts.is_empty()
                            && !self.cx.has_comment(c.span.start, c.span.end)
                        {
                            self.out.push(
                                    self.cx
                                        .warn("emptyBlock", "this `catch` is empty", c.span, "empty")
                                        .help("handle the error, or put a comment inside saying why it's ignored"),
                                );
                        }
                    }
                }
                if let ExprKind::Try { body, catches, .. } = &e.kind {
                    self.expr(body);
                    for c in catches {
                        self.pattern(&c.pat);
                        // (The catch's block itself was reported above.)
                        match &c.body.kind {
                            ExprKind::Block(b) => self.block(b),
                            _ => self.expr(&c.body),
                        }
                    }
                }
                if let Some(f) = finally {
                    self.empty(f, f.span);
                    let saved = self.finally.replace(0);
                    self.block(f);
                    self.finally = saved;
                }
                return;
            }
            ExprKind::Return(_) | ExprKind::Throw(_) | ExprKind::Break(_) | ExprKind::Continue => {
                if let Some(loops) = self.finally {
                    let leaves =
                        matches!(e.kind, ExprKind::Return(_) | ExprKind::Throw(_)) || loops == 0;
                    if leaves && self.cx.on("unsafeFinally").is_some() {
                        self.out.push(
                            self.cx
                                .warn(
                                    "unsafeFinally",
                                    "this leaves a `finally` block, replacing the `try`'s result or error",
                                    e.span,
                                    "leaves `finally`",
                                )
                                .help("move it out of the `finally` block"),
                        );
                    }
                }
            }
            ExprKind::Lit(Lit::Str(s)) if self.cx.on("templateInString").is_some() => {
                if let Some(i) = s.find("${")
                    && s[i..].contains('}')
                {
                    self.out.push(
                            self.cx
                                .warn(
                                    "templateInString",
                                    "this plain string contains `${…}`, which only a template substitutes",
                                    e.span,
                                    "not substituted",
                                )
                                .help("use a template: `…`"),
                        );
                }
            }
            ExprKind::Lit(Lit::Regex { pattern, .. })
                if self.cx.on("preferNamedCaptureGroup").is_some() =>
            {
                self.regex(pattern, e.span);
            }
            _ => {}
        }
        let is_loop = matches!(e.kind, ExprKind::Loop(_));
        if is_loop && let Some(n) = self.finally.as_mut() {
            *n += 1;
        }
        walk::walk_expr(self, e);
        if is_loop && let Some(n) = self.finally.as_mut() {
            *n -= 1;
        }
    }
}

impl Mistakes<'_, '_, '_> {
    /// Checks an `if` chain once, from its first `if` (an `else if` is checked with it).
    fn if_chain_once(&mut self, e: &Expr) {
        let in_chain = self
            .cx
            .token_before(e.span.start)
            .is_some_and(|t| t.kind == pico_syntax::TokenKind::Kw(pico_syntax::Kw::Else));
        if !in_chain {
            self.if_chain(e);
        }
    }

    fn non_null_left(&mut self, lhs: &Expr, at: Span, op: &str) {
        if optional_chain(lhs) {
            return; // `a?.b` is null when `a` is, whatever `b`'s type
        }
        let Some(t) = self.cx.ty(strip_parens(lhs).span) else {
            return;
        };
        if non_null(t) {
            self.out.push(
                self.cx
                    .warn(
                        "constantBinary",
                        format!(
                            "the left side of `{op}` can't be null, so the right side is never used"
                        ),
                        at,
                        "never null",
                    )
                    .help("remove the right side"),
            );
        }
    }
}
