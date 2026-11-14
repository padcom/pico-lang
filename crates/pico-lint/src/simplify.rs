//! Simplifications (P10.2g): `preferConst`, `objectShorthand`, `uselessRename`,
//! `operatorAssignment`, `preferTemplate`, `uselessConcat`, `arrowBodyStyle`,
//! `preferArrowCallback`, `funcStyle`, `lonelyIf`, `loneBlocks`, `uselessReturn`,
//! `uselessConstructor`, `preferDestructuring`, `preferExponentiation`, `yoda`,
//! `groupedAccessors`.

use crate::cx::Cx;
use crate::scope::{self, Kind, strip_parens};
use crate::walk::{self, Visit};
use pico_diag::{Diagnostic, Span};
use pico_sema::ty::Ty;
use pico_syntax::ast::*;
use std::collections::{BTreeMap, HashSet};

pub fn check(cx: &Cx, out: &mut Vec<Diagnostic>) {
    if cx.on("preferConst").is_some() {
        prefer_const(cx, out);
    }
    let getters: HashSet<Span> = match (&cx.checked.hir, &cx.checked.types) {
        (Some(p), Some(t)) => t
            .getters
            .keys()
            .filter_map(|id| p.expr_spans.get(id.0 as usize).copied())
            .collect(),
        _ => HashSet::new(),
    };
    let mut s = Simplify {
        cx,
        out,
        getters,
        chained: HashSet::new(),
    };
    for st in &cx.module.stmts {
        s.stmt(st);
    }
    s.lone_blocks(&cx.module.stmts);
}

fn prefer_const(cx: &Cx, out: &mut Vec<Diagnostic>) {
    let s = scope::resolve(cx.module);
    let mut groups: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for (i, b) in s.bindings.iter().enumerate() {
        if b.kind == Kind::Let && b.init && !b.special {
            groups.entry(b.group).or_default().push(i);
        }
    }
    for ids in groups.values() {
        if ids.iter().all(|&i| s.bindings[i].writes == 0) {
            let names: Vec<String> = ids
                .iter()
                .map(|&i| format!("`{}`", s.bindings[i].name))
                .collect();
            let first = &s.bindings[ids[0]];
            let msg = if names.len() == 1 {
                format!("{} is never reassigned", names[0])
            } else {
                format!("{} are never reassigned", names.join(", "))
            };
            out.push(
                cx.warn("preferConst", msg, first.span, "never reassigned")
                    .help("declare it with `const`"),
            );
        }
    }
}

struct Simplify<'c, 'a, 'o> {
    cx: &'c Cx<'a>,
    out: &'o mut Vec<Diagnostic>,
    /// Spans of member reads that call a getter.
    getters: HashSet<Span>,
    /// `+` nodes already looked at as part of a longer chain.
    chained: HashSet<Span>,
}

fn is_str(e: &Expr) -> bool {
    matches!(
        strip_parens(e).kind,
        ExprKind::Lit(Lit::Str(_)) | ExprKind::Template { tag: None, .. }
    )
}

fn is_lit(e: &Expr) -> bool {
    match &strip_parens(e).kind {
        ExprKind::Lit(_) => true,
        ExprKind::Unary {
            op: UnOp::Neg,
            expr,
        } => matches!(expr.kind, ExprKind::Lit(_)),
        _ => false,
    }
}

/// The operands of a left-leaning `a + b + c` chain.
fn add_chain<'e>(e: &'e Expr, out: &mut Vec<&'e Expr>, nodes: &mut Vec<Span>) {
    match &e.kind {
        ExprKind::Binary {
            op: BinOp::Add,
            lhs,
            rhs,
        } => {
            nodes.push(e.span);
            add_chain(lhs, out, nodes);
            out.push(rhs);
        }
        _ => out.push(e),
    }
}

fn uses_this(f: &FnDecl) -> bool {
    struct T {
        found: bool,
        root: bool,
    }
    impl<'a> Visit<'a> for T {
        fn func(&mut self, f: &'a FnDecl) {
            // Arrow functions inside see the same `this`.
            if std::mem::replace(&mut self.root, false) || f.flags.arrow {
                walk::walk_fn(self, f);
            }
        }
        fn expr(&mut self, e: &'a Expr) {
            if matches!(e.kind, ExprKind::This | ExprKind::Super) {
                self.found = true;
            }
            walk::walk_expr(self, e);
        }
    }
    let mut t = T {
        found: false,
        root: true,
    };
    t.func(f);
    t.found
}

fn names_itself(f: &FnDecl) -> bool {
    let Some(n) = &f.name else { return false };
    struct N<'n> {
        name: &'n str,
        found: bool,
    }
    impl<'a> Visit<'a> for N<'_> {
        fn expr(&mut self, e: &'a Expr) {
            if matches!(&e.kind, ExprKind::Ident(x) if x == self.name) {
                self.found = true;
            }
            walk::walk_expr(self, e);
        }
    }
    let mut v = N {
        name: &n.name,
        found: false,
    };
    walk::walk_fn(&mut v, f);
    v.found
}

impl Simplify<'_, '_, '_> {
    fn warn(&mut self, rule: &str, msg: String, span: Span, label: &str, help: &str) {
        if self.cx.on(rule).is_some() {
            let d = self.cx.warn(rule, msg, span, label).help(help.to_string());
            self.out.push(d);
        }
    }

    fn src(&self, e: &Expr) -> String {
        self.cx
            .src(strip_parens(e).span)
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect()
    }

    fn lone_blocks(&mut self, stmts: &[Stmt]) {
        if self.cx.on("loneBlocks").is_none() {
            return;
        }
        for s in stmts {
            let StmtKind::Expr { expr, .. } = &s.kind else {
                continue;
            };
            let ExprKind::Block(b) = &expr.kind else {
                continue;
            };
            let scopes = b.stmts.iter().any(|s| {
                matches!(
                    s.kind,
                    StmtKind::Let { .. } | StmtKind::Decl(_) | StmtKind::Using { .. }
                )
            });
            if !scopes {
                let at = self.cx.span(expr.span.start, expr.span.start + 1);
                self.warn(
                    "loneBlocks",
                    "this block declares nothing, so it scopes nothing".into(),
                    at,
                    "redundant block",
                    "remove the braces",
                );
            }
        }
    }

    fn plus_chain(&mut self, e: &Expr) {
        if self.chained.contains(&e.span) {
            return;
        }
        let (mut ops, mut nodes) = (Vec::new(), Vec::new());
        add_chain(e, &mut ops, &mut nodes);
        self.chained.extend(nodes);
        if !ops.iter().any(|o| is_str(o)) {
            return;
        }
        if ops.iter().any(|o| !is_str(o)) {
            self.warn(
                "preferTemplate",
                "this string is built with `+`".into(),
                e.span,
                "concatenation",
                "write it as a template: `text ${value}`",
            );
        }
        for w in ops.windows(2) {
            if is_str(w[0])
                && is_str(w[1])
                && self.cx.line(w[0].span.end) == self.cx.line(w[1].span.start)
            {
                let span = self.cx.span(w[0].span.start, w[1].span.end);
                self.warn(
                    "uselessConcat",
                    "two string literals are joined with `+`".into(),
                    span,
                    "one string",
                    "write them as one string",
                );
            }
        }
    }

    fn type_body(&mut self, members: &[Member], is_class: bool, extends: bool) {
        // groupedAccessors
        let accessor = |m: &Member, get: bool| match (&m.kind, get) {
            (MemberKind::Getter(f), true) | (MemberKind::Setter(f), false) => {
                f.name.as_ref().map(|n| (n.name.clone(), m.mods.is_static))
            }
            _ => None,
        };
        for (si, m) in members.iter().enumerate() {
            let Some(key) = accessor(m, false) else {
                continue;
            };
            if let Some(gi) = members
                .iter()
                .position(|g| accessor(g, true).as_ref() == Some(&key))
                && gi + 1 != si
            {
                let at = match &m.kind {
                    MemberKind::Setter(f) => f.name.as_ref().map_or(m.span, |n| n.span),
                    _ => m.span,
                };
                self.warn(
                    "groupedAccessors",
                    format!("the setter of `{}` isn't right after its getter", key.0),
                    at,
                    "apart from its getter",
                    "move it right after the getter",
                );
            }
        }
        // uselessConstructor
        if !is_class {
            return;
        }
        let ctors: Vec<&Member> = members
            .iter()
            .filter(|m| matches!(m.kind, MemberKind::Constructor(_)))
            .collect();
        let [m] = ctors[..] else { return };
        let MemberKind::Constructor(f) = &m.kind else {
            return;
        };
        let narrowed = matches!(
            m.mods.visibility,
            Some(Visibility::Private | Visibility::Protected)
        );
        let plain_params = f
            .params
            .iter()
            .all(|p| p.mods.is_empty() && p.decorators.is_empty() && p.default.is_none());
        if narrowed || !m.decorators.is_empty() || !plain_params {
            return;
        }
        let Some(FnBody::Block(b)) = &f.body else {
            return;
        };
        let useless = if !extends {
            b.stmts.is_empty()
        } else {
            match &b.stmts[..] {
                [Stmt {
                    kind: StmtKind::Expr { expr, .. },
                    ..
                }] => match &expr.kind {
                    ExprKind::Call { callee, args, .. } if matches!(callee.kind, ExprKind::Super) => {
                        args.len() == f.params.len()
                            && args.iter().zip(&f.params).all(|(a, p)| {
                                a.name.is_none()
                                    && a.spread == p.rest
                                    && matches!((&a.value.kind, &p.pat.kind), (ExprKind::Ident(x), PatternKind::Ident(n)) if *x == n.name)
                            })
                    }
                    _ => false,
                },
                _ => false,
            }
        };
        if useless && !self.cx.has_comment(f.span.start, f.span.end) {
            self.warn(
                "uselessConstructor",
                "this constructor does nothing the default one wouldn't".into(),
                crate::complexity::first_line(self.cx.text, m.span),
                "useless",
                "remove it",
            );
        }
    }
}

impl<'a> Visit<'a> for Simplify<'_, '_, '_> {
    fn block(&mut self, b: &'a Block) {
        self.lone_blocks(&b.stmts);
        walk::walk_block(self, b);
    }

    fn decl(&mut self, d: &'a Decl) {
        match &d.kind {
            DeclKind::Class(c) => self.type_body(&c.members, true, c.extends.is_some()),
            DeclKind::Struct(s) => self.type_body(&s.members, false, false),
            DeclKind::Interface(i) => self.type_body(&i.members, false, false),
            DeclKind::Enum(e) => self.type_body(&e.members, false, false),
            DeclKind::Import(ImportDecl {
                kind: ImportKind::Named(names),
                ..
            })
            | DeclKind::ExportList { names, .. } => {
                for n in names {
                    if let Some(a) = n.alias.as_ref().filter(|a| a.name == n.name.name) {
                        self.warn(
                            "uselessRename",
                            format!("`{}` is renamed to itself", a.name),
                            self.cx.span(n.name.span.start, a.span.end),
                            "same name",
                            "drop the `as …`",
                        );
                    }
                }
            }
            _ => {}
        }
        walk::walk_decl(self, d);
    }

    fn func(&mut self, f: &'a FnDecl) {
        if let Some(FnBody::Block(b)) = &f.body {
            if let Some(Stmt {
                kind: StmtKind::Expr { expr, .. },
                ..
            }) = b.stmts.last()
                && matches!(expr.kind, ExprKind::Return(None))
            {
                self.warn(
                    "uselessReturn",
                    "this `return` is at the end of the function anyway".into(),
                    expr.span,
                    "useless",
                    "remove it",
                );
            }
            if f.flags.arrow
                && let [
                    Stmt {
                        kind: StmtKind::Expr { expr, .. },
                        ..
                    },
                ] = &b.stmts[..]
                && let ExprKind::Return(Some(x)) = &expr.kind
                && !matches!(
                    strip_parens(x).kind,
                    ExprKind::Object { .. } | ExprKind::Block(_)
                )
            {
                self.warn(
                    "arrowBodyStyle",
                    "this arrow function's block only returns a value".into(),
                    crate::complexity::first_line(self.cx.text, f.span),
                    "block body",
                    "write the value after `=>`, without braces and `return`",
                );
            }
        }
        walk::walk_fn(self, f);
    }

    fn pattern(&mut self, p: &'a Pattern) {
        let fields = match &p.kind {
            PatternKind::Struct { fields, .. } => fields,
            PatternKind::Typed {
                fields: Some(fields),
                ..
            } => fields,
            _ => return walk_pattern(self, p),
        };
        for f in fields {
            if let Some(Pattern {
                kind: PatternKind::Ident(n),
                span,
            }) = &f.pat
                && n.name == f.name.name
            {
                self.warn(
                    "uselessRename",
                    format!("`{}` is renamed to itself", n.name),
                    self.cx.span(f.name.span.start, span.end),
                    "same name",
                    &format!("write just `{}`", n.name),
                );
            }
        }
        walk_pattern(self, p);
    }

    fn stmt(&mut self, s: &'a Stmt) {
        if let StmtKind::Let {
            pat,
            ty,
            init: Some(init),
            ..
        } = &s.kind
        {
            if let ExprKind::Function(f) = &init.kind
                && !f.flags.arrow
            {
                self.warn(
                    "funcStyle",
                    "a `function` expression is assigned to a variable".into(),
                    crate::complexity::first_line(self.cx.text, s.span),
                    "function expression",
                    "declare a function (`function name(…) { … }`), or use an arrow function",
                );
            }
            if let (
                PatternKind::Ident(n),
                None,
                ExprKind::Member {
                    object,
                    prop,
                    optional: false,
                },
            ) = (&pat.kind, ty, &init.kind)
            {
                let typed = self
                    .cx
                    .ty(strip_parens(object).span)
                    .is_some_and(|t| matches!(t, Ty::Struct(..) | Ty::Class(..)));
                if n.name == prop.name
                    && typed
                    && !matches!(object.kind, ExprKind::This | ExprKind::Super)
                    && !self.getters.contains(&init.span)
                {
                    self.warn(
                        "preferDestructuring",
                        format!("`{}` is read from a field of the same name", n.name),
                        s.span,
                        "use destructuring",
                        &format!(
                            "write `const {{ {} }} = {}`",
                            n.name,
                            self.cx.src(object.span)
                        ),
                    );
                }
            }
        }
        walk::walk_stmt(self, s);
    }

    fn expr(&mut self, e: &'a Expr) {
        match &e.kind {
            ExprKind::Object { fields, .. } => {
                for f in fields {
                    if let ObjField::KeyValue {
                        key: PropKey::Ident(k),
                        value,
                    } = f
                        && matches!(&value.kind, ExprKind::Ident(v) if *v == k.name)
                    {
                        self.warn(
                            "objectShorthand",
                            format!("`{0}: {0}` repeats the name", k.name),
                            self.cx.span(k.span.start, value.span.end),
                            "longhand",
                            &format!("write just `{}`", k.name),
                        );
                    }
                }
            }
            ExprKind::Assign {
                op: None,
                target,
                value,
            } => {
                if let ExprKind::Binary { op, lhs, .. } = &strip_parens(value).kind {
                    let arith = matches!(
                        op,
                        BinOp::Add
                            | BinOp::Sub
                            | BinOp::Mul
                            | BinOp::Div
                            | BinOp::Rem
                            | BinOp::Pow
                            | BinOp::Shl
                            | BinOp::Shr
                            | BinOp::UShr
                            | BinOp::BitAnd
                            | BinOp::BitOr
                            | BinOp::BitXor
                    );
                    if arith && self.src(lhs) == self.src(target) {
                        self.warn(
                            "operatorAssignment",
                            format!(
                                "`{0} = {0} {1} …` where `{0} {1}= …` would do",
                                self.cx.src(target.span),
                                op.as_str()
                            ),
                            e.span,
                            "can be shorter",
                            &format!("write `{} {}= …`", self.cx.src(target.span), op.as_str()),
                        );
                    }
                }
            }
            ExprKind::Binary { op, lhs, rhs } => {
                if *op == BinOp::Add {
                    self.plus_chain(e);
                }
                let cmp = matches!(
                    op,
                    BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge
                );
                if cmp && is_lit(lhs) && !is_lit(rhs) {
                    self.warn(
                        "yoda",
                        "the literal comes first in this comparison".into(),
                        e.span,
                        "literal first",
                        "put the value being tested first",
                    );
                }
            }
            ExprKind::If {
                else_: Some(else_), ..
            } => {
                if let ExprKind::Block(b) = &else_.kind
                    && let [
                        Stmt {
                            kind: StmtKind::Expr { expr, semi: false },
                            ..
                        },
                    ] = &b.stmts[..]
                    && matches!(expr.kind, ExprKind::If { .. })
                    && !self.cx.has_comment(else_.span.start, else_.span.end)
                {
                    self.warn(
                        "lonelyIf",
                        "this `if` is alone in an `else` block".into(),
                        self.cx.span(expr.span.start, expr.span.start + 2),
                        "lonely",
                        "write `else if` instead",
                    );
                }
            }
            ExprKind::Call { callee, args, .. } => {
                if let ExprKind::Member { object, prop, .. } = &callee.kind
                    && prop.name == "pow"
                    && matches!(&object.kind, ExprKind::Ident(m) if m == "Math")
                    && args.len() == 2
                {
                    self.warn(
                        "preferExponentiation",
                        "`Math.pow(a, b)` where `a ** b` would do".into(),
                        e.span,
                        "use `**`",
                        "write `a ** b`",
                    );
                }
                for a in args {
                    if let ExprKind::Function(f) = &a.value.kind
                        && !f.flags.arrow
                        && !f.flags.generator
                        && !uses_this(f)
                        && !names_itself(f)
                    {
                        self.warn(
                            "preferArrowCallback",
                            "a `function` expression is passed as a callback".into(),
                            crate::complexity::first_line(self.cx.text, a.value.span),
                            "function expression",
                            "pass an arrow function",
                        );
                    }
                }
            }
            _ => {}
        }
        walk::walk_expr(self, e);
    }
}

/// Visits the patterns nested in `p` (field and element patterns).
fn walk_pattern<'a, V: Visit<'a> + ?Sized>(v: &mut V, p: &'a Pattern) {
    match &p.kind {
        PatternKind::Array(ps) | PatternKind::Or(ps) => ps.iter().for_each(|p| v.pattern(p)),
        PatternKind::TupleStruct { elems, .. } => elems.iter().for_each(|p| v.pattern(p)),
        PatternKind::Struct { fields, .. }
        | PatternKind::Typed {
            fields: Some(fields),
            ..
        } => fields
            .iter()
            .filter_map(|f| f.pat.as_ref())
            .for_each(|p| v.pattern(p)),
        PatternKind::As { pat, .. } | PatternKind::Default { pat, .. } => v.pattern(pat),
        _ => {}
    }
}
