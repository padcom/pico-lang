//! S-expression dump of the AST (`pico parse --dump`). Types are printed in source syntax; lists
//! that don't fit in [`WIDTH`] columns are broken one child per line.

use crate::ast::*;

const WIDTH: usize = 90;

/// A tiny S-expression tree.
#[derive(Clone, Debug)]
pub enum S {
    Atom(String),
    List(Vec<S>),
}

fn atom(s: impl Into<String>) -> S {
    S::Atom(s.into())
}

fn list(head: &str, mut rest: Vec<S>) -> S {
    rest.insert(0, atom(head));
    S::List(rest)
}

impl S {
    fn flat(&self) -> String {
        match self {
            S::Atom(a) => a.clone(),
            S::List(items) => {
                let inner: Vec<String> = items.iter().map(S::flat).collect();
                format!("({})", inner.join(" "))
            }
        }
    }

    fn pretty(&self, indent: usize, out: &mut String) {
        let flat = self.flat();
        match self {
            S::List(items) if indent + flat.len() > WIDTH && items.len() > 1 => {
                out.push('(');
                out.push_str(&items[0].flat());
                for item in &items[1..] {
                    out.push('\n');
                    out.push_str(&" ".repeat(indent + 2));
                    item.pretty(indent + 2, out);
                }
                out.push(')');
            }
            _ => out.push_str(&flat),
        }
    }

    pub fn to_pretty(&self) -> String {
        let mut out = String::new();
        self.pretty(0, &mut out);
        out
    }
}

/// Dumps a module: one top-level S-expression per statement.
pub fn dump_module(m: &Module) -> String {
    let mut out = String::new();
    for stmt in &m.stmts {
        out.push_str(&stmt_s(stmt).to_pretty());
        out.push('\n');
    }
    out
}

pub fn dump_expr(e: &Expr) -> String {
    expr_s(e).to_pretty()
}

pub fn dump_type(t: &Type) -> String {
    type_str(t)
}

// ---------------------------------------------------------------- literals and types

fn lit_str(l: &Lit) -> String {
    match l {
        Lit::Int { value, suffix } => {
            format!("{value}{}", suffix.map_or("", |s| s.as_str()))
        }
        Lit::Float { value, suffix } => {
            format!("{value:?}{}", suffix.map_or("", |s| s.as_str()))
        }
        Lit::Str(s) => format!("{s:?}"),
        Lit::Bool(b) => b.to_string(),
        Lit::Null => "null".into(),
        Lit::Regex { pattern, flags } => format!("/{pattern}/{flags}"),
    }
}

fn path_str(p: &Path) -> String {
    p.iter()
        .map(|i| i.name.as_str())
        .collect::<Vec<_>>()
        .join(".")
}

/// Types in source syntax, fully parenthesized where precedence could be unclear.
pub fn type_str(t: &Type) -> String {
    match &t.kind {
        TypeKind::Path { path, args } if args.is_empty() => path_str(path),
        TypeKind::Path { path, args } => {
            let a: Vec<String> = args.iter().map(type_str).collect();
            format!("{}<{}>", path_str(path), a.join(", "))
        }
        TypeKind::Array(e) => format!("{}[]", type_atomic(e)),
        TypeKind::Fixed { elem, len } => format!("[{}; {}]", type_str(elem), expr_s(len).flat()),
        TypeKind::Tuple(ts) => {
            format!(
                "[{}]",
                ts.iter().map(type_str).collect::<Vec<_>>().join(", ")
            )
        }
        TypeKind::Nullable(e) => format!("{}?", type_atomic(e)),
        TypeKind::Union(ts) => ts.iter().map(type_atomic).collect::<Vec<_>>().join(" | "),
        TypeKind::Intersection(ts) => ts.iter().map(type_atomic).collect::<Vec<_>>().join(" & "),
        TypeKind::Function {
            params,
            ret,
            throws,
            extern_abi,
        } => {
            let ps: Vec<String> = params
                .iter()
                .map(|p| {
                    let mut s = String::new();
                    if p.rest {
                        s.push_str("...");
                    }
                    if let Some(n) = &p.name {
                        s.push_str(&n.name);
                        if p.optional {
                            s.push('?');
                        }
                        s.push_str(": ");
                    }
                    s.push_str(&type_str(&p.ty));
                    s
                })
                .collect();
            let mut s = String::new();
            if let Some(abi) = extern_abi {
                s.push_str(&format!("extern {abi:?} "));
            }
            s.push_str(&format!("({}) => {}", ps.join(", "), type_str(ret)));
            if let Some(t) = throws {
                s.push_str(&format!(" throws {}", type_str(t)));
            }
            s
        }
        TypeKind::Object(fields) => {
            let fs: Vec<String> = fields
                .iter()
                .map(|f| {
                    format!(
                        "{}{}{}: {}",
                        if f.readonly { "readonly " } else { "" },
                        f.name.name,
                        if f.optional { "?" } else { "" },
                        type_str(&f.ty)
                    )
                })
                .collect();
            format!("{{ {} }}", fs.join(", "))
        }
        TypeKind::Lit(l) => lit_str(l),
        TypeKind::Immutable(t) => format!("immutable {}", type_atomic(t)),
        TypeKind::Distinct(t) => format!("distinct {}", type_str(t)),
        TypeKind::Const(e) => expr_s(e).flat(),
        TypeKind::Error => "<error>".into(),
    }
}

fn type_atomic(t: &Type) -> String {
    match &t.kind {
        TypeKind::Union(_)
        | TypeKind::Intersection(_)
        | TypeKind::Function { .. }
        | TypeKind::Immutable(_)
        | TypeKind::Distinct(_) => format!("({})", type_str(t)),
        _ => type_str(t),
    }
}

fn ty_s(t: &Type) -> S {
    atom(type_str(t))
}

// ---------------------------------------------------------------- expressions

fn args_s(args: &[Arg]) -> Vec<S> {
    args.iter()
        .map(|a| {
            let v = expr_s(&a.value);
            if a.spread {
                list("...", vec![v])
            } else if let Some(n) = &a.name {
                list(&format!("{}:", n.name), vec![v])
            } else {
                v
            }
        })
        .collect()
}

fn targs_s(ts: &[Type]) -> Option<S> {
    if ts.is_empty() {
        None
    } else {
        Some(list("targs", ts.iter().map(ty_s).collect()))
    }
}

fn block_s(b: &Block) -> S {
    list("block", b.stmts.iter().map(stmt_s).collect())
}

pub fn expr_s(e: &Expr) -> S {
    match &e.kind {
        ExprKind::Lit(l) => atom(lit_str(l)),
        ExprKind::Ident(n) => atom(n.clone()),
        ExprKind::This => atom("this"),
        ExprKind::Super => atom("super"),
        ExprKind::Template { tag, quasis, exprs } => {
            let mut items = Vec::new();
            if let Some(t) = tag {
                items.push(list("tag", vec![expr_s(t)]));
            }
            for (i, q) in quasis.iter().enumerate() {
                if !q.is_empty() {
                    items.push(atom(format!("{q:?}")));
                }
                if let Some(x) = exprs.get(i) {
                    let mut sub = vec![expr_s(&x.expr)];
                    if let Some(spec) = &x.spec {
                        sub.push(atom(format!(":{spec}")));
                    }
                    items.push(list("${}", sub));
                }
            }
            list("template", items)
        }
        ExprKind::Array(elems) => list(
            "array",
            elems
                .iter()
                .map(|el| match el {
                    ArrayElem::Expr(e) => expr_s(e),
                    ArrayElem::Spread(e) => list("...", vec![expr_s(e)]),
                })
                .collect(),
        ),
        ExprKind::ArrayRepeat { value, count } => {
            list("array-repeat", vec![expr_s(value), expr_s(count)])
        }
        ExprKind::Object { ty, fields } => {
            let mut items: Vec<S> = ty.iter().map(|t| ty_s(t)).collect();
            for f in fields {
                items.push(match f {
                    ObjField::KeyValue { key, value } => {
                        let k = match key {
                            PropKey::Ident(i) => i.name.clone(),
                            PropKey::Str(s, _) => format!("{s:?}"),
                        };
                        list(&format!("{k}:"), vec![expr_s(value)])
                    }
                    ObjField::Shorthand(i) => atom(i.name.clone()),
                    ObjField::Spread(e) => list("...", vec![expr_s(e)]),
                });
            }
            list("object", items)
        }
        ExprKind::Paren(e) => list("paren", vec![expr_s(e)]),
        ExprKind::Unary { op, expr } => list(op.as_str(), vec![expr_s(expr)]),
        ExprKind::Binary { op, lhs, rhs } => list(op.as_str(), vec![expr_s(lhs), expr_s(rhs)]),
        ExprKind::Assign { op, target, value } => {
            let head = match op {
                None => "=".to_string(),
                Some(op) => format!("{}=", op.as_str()),
            };
            list(&head, vec![expr_s(target), expr_s(value)])
        }
        ExprKind::Ternary { cond, then, else_ } => {
            list("?:", vec![expr_s(cond), expr_s(then), expr_s(else_)])
        }
        ExprKind::Call {
            callee,
            type_args,
            args,
        } => {
            let mut items = vec![expr_s(callee)];
            items.extend(targs_s(type_args));
            items.extend(args_s(args));
            list("call", items)
        }
        ExprKind::New {
            callee,
            type_args,
            args,
        } => {
            let mut items = vec![expr_s(callee)];
            items.extend(targs_s(type_args));
            items.extend(args_s(args));
            list("new", items)
        }
        ExprKind::Member {
            object,
            prop,
            optional,
        } => list(
            if *optional { "?." } else { "." },
            vec![expr_s(object), atom(prop.name.clone())],
        ),
        ExprKind::Index { object, index } => list("index", vec![expr_s(object), expr_s(index)]),
        ExprKind::NonNull(e) => list("non-null", vec![expr_s(e)]),
        ExprKind::As { expr, ty } => list("as", vec![expr_s(expr), ty_s(ty)]),
        ExprKind::Is { expr, pat } => list("is", vec![expr_s(expr), pat_s(pat)]),
        ExprKind::Instanceof { expr, ty } => list("instanceof", vec![expr_s(expr), ty_s(ty)]),
        ExprKind::Range { start, end } => list("..", vec![expr_s(start), expr_s(end)]),
        ExprKind::Function(f) => fn_s(if f.flags.arrow { "arrow" } else { "fn" }, f),
        ExprKind::Block(b) => block_s(b),
        ExprKind::If { cond, then, else_ } => {
            let mut items = vec![expr_s(cond), expr_s(then)];
            if let Some(e) = else_ {
                items.push(expr_s(e));
            }
            list("if", items)
        }
        ExprKind::Match { scrutinee, arms } => {
            let mut items = vec![expr_s(scrutinee)];
            for arm in arms {
                let mut a = vec![pat_s(&arm.pat)];
                if let Some(g) = &arm.guard {
                    a.push(list("if", vec![expr_s(g)]));
                }
                a.push(expr_s(&arm.body));
                items.push(list("=>", a));
            }
            list("match", items)
        }
        ExprKind::Switch { scrutinee, cases } => {
            let mut items = vec![expr_s(scrutinee)];
            for c in cases {
                let mut a = Vec::new();
                let head = match &c.pat {
                    Some(p) => {
                        a.push(pat_s(p));
                        "case"
                    }
                    None => "default",
                };
                if let Some(g) = &c.guard {
                    a.push(list("if", vec![expr_s(g)]));
                }
                a.push(block_s(&c.body));
                items.push(list(head, a));
            }
            list("switch", items)
        }
        ExprKind::Loop(b) => list("loop", vec![block_s(b)]),
        ExprKind::Try {
            body,
            catches,
            finally,
        } => {
            let mut items = vec![expr_s(body)];
            for c in catches {
                items.push(list("catch", vec![pat_s(&c.pat), expr_s(&c.body)]));
            }
            if let Some(f) = finally {
                items.push(list("finally", vec![block_s(f)]));
            }
            list("try", items)
        }
        ExprKind::TryOpt(e) => list("try?", vec![expr_s(e)]),
        ExprKind::TryBang(e) => list("try!", vec![expr_s(e)]),
        ExprKind::Throw(e) => list("throw", vec![expr_s(e)]),
        ExprKind::Return(e) => list("return", e.iter().map(|e| expr_s(e)).collect()),
        ExprKind::Break(e) => list("break", e.iter().map(|e| expr_s(e)).collect()),
        ExprKind::Continue => atom("continue"),
        ExprKind::Await(e) => list("await", vec![expr_s(e)]),
        ExprKind::Yield { expr, delegate } => list(
            if *delegate { "yield*" } else { "yield" },
            expr.iter().map(|e| expr_s(e)).collect(),
        ),
        ExprKind::Tailcall(e) => list("tailcall", vec![expr_s(e)]),
        ExprKind::Comptime(e) => list("comptime", vec![expr_s(e)]),
        ExprKind::Unsafe(b) => list("unsafe", vec![block_s(b)]),
        ExprKind::Arena(b) => list("arena", vec![block_s(b)]),
        ExprKind::Error => atom("<error>"),
    }
}

// ---------------------------------------------------------------- patterns

fn field_pats_s(fields: &[FieldPat]) -> Vec<S> {
    fields
        .iter()
        .map(|f| {
            let base = match &f.pat {
                None => atom(f.name.name.clone()),
                Some(p) => list(&format!("{}:", f.name.name), vec![pat_s(p)]),
            };
            match &f.default {
                None => base,
                Some(d) => list("=", vec![base, expr_s(d)]),
            }
        })
        .collect()
}

pub fn pat_s(p: &Pattern) -> S {
    match &p.kind {
        PatternKind::Wildcard => atom("_"),
        PatternKind::Ident(i) => atom(i.name.clone()),
        PatternKind::Path(path) => atom(path_str(path)),
        PatternKind::Lit(l) => atom(lit_str(l)),
        PatternKind::Neg(p) => atom(format!("-{}", pat_s(p).flat())),
        PatternKind::Range { start, end } => list("..", vec![pat_s(start), pat_s(end)]),
        PatternKind::TupleStruct { path, elems } => {
            let mut items = vec![atom(path_str(path))];
            items.extend(elems.iter().map(pat_s));
            list("variant", items)
        }
        PatternKind::Struct { path, fields, rest } => {
            let mut items = Vec::new();
            let head = match path {
                Some(p) => {
                    items.push(atom(path_str(p)));
                    "struct-pat"
                }
                None => "object-pat",
            };
            items.extend(field_pats_s(fields));
            match rest {
                Some(Some(r)) => items.push(list("...", vec![atom(r.name.clone())])),
                Some(None) => items.push(atom("...")),
                None => {}
            }
            list(head, items)
        }
        PatternKind::Array(elems) => list("array-pat", elems.iter().map(pat_s).collect()),
        PatternKind::Rest(Some(r)) => list("...", vec![atom(r.name.clone())]),
        PatternKind::Rest(None) => atom("..."),
        PatternKind::Or(ps) => list("|", ps.iter().map(pat_s).collect()),
        PatternKind::Typed { name, ty, fields } => {
            let mut items = vec![atom(name.name.clone()), ty_s(ty)];
            if let Some(fs) = fields {
                items.extend(field_pats_s(fs));
            }
            list("typed", items)
        }
        PatternKind::As { name, pat } => list("as", vec![atom(name.name.clone()), pat_s(pat)]),
        PatternKind::Default { pat, default } => list("=", vec![pat_s(pat), expr_s(default)]),
        PatternKind::Error => atom("<error>"),
    }
}

// ---------------------------------------------------------------- statements

pub fn stmt_s(s: &Stmt) -> S {
    match &s.kind {
        StmtKind::Expr { expr, semi } => {
            let e = expr_s(expr);
            if *semi { list(";", vec![e]) } else { e }
        }
        StmtKind::Let {
            kind,
            pat,
            ty,
            init,
            comptime,
        } => {
            let head = match (kind, comptime) {
                (BindingKind::Const, false) => "const",
                (BindingKind::Const, true) => "comptime-const",
                (BindingKind::Let, false) => "let",
                (BindingKind::Let, true) => "comptime-let",
            };
            let mut items = vec![pat_s(pat)];
            if let Some(t) = ty {
                items.push(list(":", vec![ty_s(t)]));
            }
            if let Some(i) = init {
                items.push(expr_s(i));
            }
            list(head, items)
        }
        StmtKind::Using {
            is_await,
            name,
            init,
        } => list(
            if *is_await { "await-using" } else { "using" },
            vec![atom(name.name.clone()), expr_s(init)],
        ),
        StmtKind::While { cond, body } => list("while", vec![expr_s(cond), expr_s(body)]),
        StmtKind::DoWhile { body, cond } => list("do-while", vec![block_s(body), expr_s(cond)]),
        StmtKind::For {
            init,
            cond,
            update,
            body,
        } => list(
            "for",
            vec![
                init.as_ref().map_or(atom("_"), |s| stmt_s(s)),
                cond.as_ref().map_or(atom("_"), expr_s),
                update.as_ref().map_or(atom("_"), expr_s),
                expr_s(body),
            ],
        ),
        StmtKind::ForOf {
            is_await,
            kind,
            pat,
            iter,
            body,
        } => list(
            if *is_await { "for-await-of" } else { "for-of" },
            vec![
                atom(match kind {
                    BindingKind::Const => "const",
                    BindingKind::Let => "let",
                }),
                pat_s(pat),
                expr_s(iter),
                expr_s(body),
            ],
        ),
        StmtKind::Decl(d) => decl_s(d),
        StmtKind::Error => atom("<error>"),
    }
}

// ---------------------------------------------------------------- declarations

fn decorators_s(ds: &[Decorator]) -> Vec<S> {
    ds.iter()
        .map(|d| {
            let name = format!("@{}", path_str(&d.path));
            match &d.args {
                Some(a) => list(&name, args_s(a)),
                None => atom(name),
            }
        })
        .collect()
}

fn mods_words(m: &Modifiers) -> Vec<S> {
    let mut w = Vec::new();
    match m.visibility {
        Some(Visibility::Public) => w.push(atom("public")),
        Some(Visibility::Protected) => w.push(atom("protected")),
        Some(Visibility::Private) => w.push(atom("private")),
        None => {}
    }
    for (on, name) in [
        (m.is_static, "static"),
        (m.abstract_, "abstract"),
        (m.final_, "final"),
        (m.virtual_, "virtual"),
        (m.override_, "override"),
        (m.readonly, "readonly"),
        (m.is_async, "async"),
    ] {
        if on {
            w.push(atom(name));
        }
    }
    w
}

fn generics_s(gs: &[GenericParam]) -> Option<S> {
    if gs.is_empty() {
        return None;
    }
    Some(list(
        "generics",
        gs.iter()
            .map(|g| match &g.kind {
                GenericKind::Type {
                    bound: None,
                    default: None,
                } => atom(g.name.name.clone()),
                GenericKind::Type { bound, default } => {
                    let mut items = vec![atom(g.name.name.clone())];
                    if let Some(b) = bound {
                        items.push(list("extends", vec![ty_s(b)]));
                    }
                    if let Some(d) = default {
                        items.push(list("=", vec![ty_s(d)]));
                    }
                    S::List(items)
                }
                GenericKind::Comptime { ty } => {
                    list("comptime", vec![atom(g.name.name.clone()), ty_s(ty)])
                }
            })
            .collect(),
    ))
}

fn param_s(p: &Param) -> S {
    let mut items = decorators_s(&p.decorators);
    items.extend(mods_words(&p.mods));
    if p.comptime {
        items.push(atom("comptime"));
    }
    if p.rest {
        items.push(atom("..."));
    }
    items.push(pat_s(&p.pat));
    if p.optional {
        items.push(atom("?"));
    }
    if let Some(t) = &p.ty {
        items.push(ty_s(t));
    }
    if let Some(d) = &p.default {
        items.push(list("=", vec![expr_s(d)]));
    }
    if items.len() == 1 {
        items.pop().unwrap()
    } else {
        S::List(items)
    }
}

fn fn_s(head: &str, f: &FnDecl) -> S {
    let mut items = Vec::new();
    if let Some(n) = &f.name {
        items.push(atom(n.name.clone()));
    }
    for (on, name) in [
        (f.flags.is_async, "async"),
        (f.flags.generator, "generator"),
        (f.flags.is_unsafe, "unsafe"),
        (f.flags.decorator, "decorator"),
    ] {
        if on {
            items.push(atom(name));
        }
    }
    items.extend(generics_s(&f.generics));
    items.push(list("params", f.params.iter().map(param_s).collect()));
    if let Some(r) = &f.ret {
        items.push(list("->", vec![ty_s(r)]));
    }
    if let Some(t) = &f.throws {
        items.push(list("throws", vec![ty_s(t)]));
    }
    if !f.where_clause.is_empty() {
        items.push(list(
            "where",
            f.where_clause
                .iter()
                .map(|w| list(":", vec![ty_s(&w.ty), ty_s(&w.bound)]))
                .collect(),
        ));
    }
    match &f.body {
        Some(FnBody::Block(b)) => items.push(block_s(b)),
        Some(FnBody::Expr(e)) => items.push(expr_s(e)),
        None => {}
    }
    list(head, items)
}

fn member_s(m: &Member) -> S {
    let mut pre = decorators_s(&m.decorators);
    pre.extend(mods_words(&m.mods));
    let body = match &m.kind {
        MemberKind::Field {
            name,
            ty,
            optional,
            init,
        } => {
            let mut items = pre;
            items.push(atom(format!(
                "{}{}",
                name.name,
                if *optional { "?" } else { "" }
            )));
            if let Some(t) = ty {
                items.push(ty_s(t));
            }
            if let Some(i) = init {
                items.push(list("=", vec![expr_s(i)]));
            }
            return list("field", items);
        }
        MemberKind::Method(f) => fn_s("method", f),
        MemberKind::Getter(f) => fn_s("get", f),
        MemberKind::Setter(f) => fn_s("set", f),
        MemberKind::Constructor(f) => fn_s("constructor", f),
        MemberKind::Operator { op, func } => fn_s(&format!("operator{op}"), func),
        MemberKind::StaticBlock(b) => list("static-block", vec![block_s(b)]),
    };
    if pre.is_empty() {
        body
    } else {
        let S::List(mut items) = body else {
            unreachable!()
        };
        let head = items.remove(0);
        let mut v = vec![head];
        v.extend(pre);
        v.extend(items);
        S::List(v)
    }
}

fn type_list(head: &str, ts: &[Type]) -> Option<S> {
    if ts.is_empty() {
        None
    } else {
        Some(list(head, ts.iter().map(ty_s).collect()))
    }
}

fn import_names_s(names: &[ImportName]) -> Vec<S> {
    names
        .iter()
        .map(|n| match &n.alias {
            None => atom(n.name.name.clone()),
            Some(a) => list("as", vec![atom(n.name.name.clone()), atom(a.name.clone())]),
        })
        .collect()
}

pub fn decl_s(d: &Decl) -> S {
    let inner = match &d.kind {
        DeclKind::Function(f) => fn_s("function", f),
        DeclKind::Struct(s) => {
            let mut items = vec![atom(s.name.name.clone())];
            items.extend(generics_s(&s.generics));
            items.extend(type_list("implements", &s.implements));
            items.extend(s.members.iter().map(member_s));
            list("struct", items)
        }
        DeclKind::Enum(e) => {
            let mut items = vec![atom(e.name.name.clone())];
            items.extend(generics_s(&e.generics));
            items.extend(type_list("implements", &e.implements));
            for v in &e.variants {
                let mut vi = decorators_s(&v.decorators);
                vi.push(atom(v.name.name.clone()));
                if let Some(fs) = &v.fields {
                    vi.extend(fs.iter().map(param_s));
                }
                items.push(list("variant", vi));
            }
            items.extend(e.members.iter().map(member_s));
            list("enum", items)
        }
        DeclKind::Class(c) => {
            let mut items = Vec::new();
            if c.abstract_ {
                items.push(atom("abstract"));
            }
            if c.final_ {
                items.push(atom("final"));
            }
            items.push(atom(c.name.name.clone()));
            items.extend(generics_s(&c.generics));
            if let Some(e) = &c.extends {
                items.push(list("extends", vec![ty_s(e)]));
            }
            items.extend(type_list("implements", &c.implements));
            items.extend(c.members.iter().map(member_s));
            list("class", items)
        }
        DeclKind::Interface(i) => {
            let mut items = vec![atom(i.name.name.clone())];
            items.extend(generics_s(&i.generics));
            items.extend(type_list("extends", &i.extends));
            items.extend(i.members.iter().map(member_s));
            list("interface", items)
        }
        DeclKind::TypeAlias { name, generics, ty } => {
            let mut items = vec![atom(name.name.clone())];
            items.extend(generics_s(generics));
            items.push(ty_s(ty));
            list("type", items)
        }
        DeclKind::Import(i) => {
            let mut items = match &i.kind {
                ImportKind::Named(names) => import_names_s(names),
                ImportKind::Namespace(n) => vec![list("*", vec![atom(n.name.clone())])],
            };
            items.push(list("from", vec![atom(format!("{:?}", i.from))]));
            list("import", items)
        }
        DeclKind::ExportList { names, from } => {
            let mut items = import_names_s(names);
            if let Some(f) = from {
                items.push(list("from", vec![atom(format!("{f:?}"))]));
            }
            list("export-list", items)
        }
        DeclKind::ExportAll { from } => list("export-all", vec![atom(format!("{from:?}"))]),
        DeclKind::Extern { abi, items } => {
            let mut v = vec![atom(format!("{abi:?}"))];
            v.extend(items.iter().map(decl_s));
            list("extern", v)
        }
        DeclKind::Binding(s) => stmt_s(s),
    };
    let mut wrapped = inner;
    if !d.decorators.is_empty() {
        let mut items = decorators_s(&d.decorators);
        items.push(wrapped);
        wrapped = list("decorated", items);
    }
    if d.exported {
        wrapped = list("export", vec![wrapped]);
    }
    wrapped
}

#[cfg(test)]
mod tests {
    use super::*;
    use pico_diag::{FileId, Span};

    fn sp() -> Span {
        Span::new(FileId(0), 0, 0)
    }
    fn id(n: &str) -> Ident {
        Ident {
            name: n.into(),
            span: sp(),
        }
    }
    fn e(kind: ExprKind) -> Expr {
        Expr { kind, span: sp() }
    }
    fn int(v: u64) -> Expr {
        e(ExprKind::Lit(Lit::Int {
            value: v,
            suffix: None,
        }))
    }
    fn name(n: &str) -> Expr {
        e(ExprKind::Ident(n.into()))
    }
    fn bin(op: BinOp, l: Expr, r: Expr) -> Expr {
        e(ExprKind::Binary {
            op,
            lhs: Box::new(l),
            rhs: Box::new(r),
        })
    }
    fn ty(n: &str) -> Type {
        Type {
            kind: TypeKind::Path {
                path: vec![id(n)],
                args: vec![],
            },
            span: sp(),
        }
    }
    fn t(kind: TypeKind) -> Type {
        Type { kind, span: sp() }
    }

    #[test]
    fn expressions() {
        let x = bin(BinOp::Add, int(1), bin(BinOp::Mul, int(2), int(3)));
        assert_eq!(dump_expr(&x), "(+ 1 (* 2 3))");
        let call = e(ExprKind::Call {
            callee: Box::new(e(ExprKind::Member {
                object: Box::new(name("JSON")),
                prop: id("parse"),
                optional: false,
            })),
            type_args: vec![ty("Config")],
            args: vec![
                Arg {
                    name: None,
                    spread: false,
                    value: name("text"),
                },
                Arg {
                    name: Some(id("strict")),
                    spread: false,
                    value: name("true"),
                },
            ],
        });
        assert_eq!(
            dump_expr(&call),
            "(call (. JSON parse) (targs Config) text (strict: true))"
        );
        let tpl = e(ExprKind::Template {
            tag: None,
            quasis: vec!["x = ".into(), "".into()],
            exprs: vec![TemplateExpr {
                expr: name("x"),
                spec: Some(".2f".into()),
            }],
        });
        assert_eq!(dump_expr(&tpl), r#"(template "x = " (${} x :.2f))"#);
    }

    #[test]
    fn types() {
        let map = t(TypeKind::Path {
            path: vec![id("Map")],
            args: vec![ty("string"), ty("i32")],
        });
        assert_eq!(type_str(&map), "Map<string, i32>");
        let u = t(TypeKind::Union(vec![ty("A"), ty("B")]));
        assert_eq!(
            type_str(&t(TypeKind::Array(Box::new(u.clone())))),
            "(A | B)[]"
        );
        assert_eq!(
            type_str(&t(TypeKind::Nullable(Box::new(ty("string"))))),
            "string?"
        );
        let f = t(TypeKind::Function {
            params: vec![FnTypeParam {
                name: Some(id("x")),
                ty: ty("T"),
                optional: false,
                rest: false,
            }],
            ret: Box::new(ty("U")),
            throws: Some(Box::new(ty("E"))),
            extern_abi: None,
        });
        assert_eq!(type_str(&f), "(x: T) => U throws E");
        let fixed = t(TypeKind::Fixed {
            elem: Box::new(ty("u8")),
            len: Box::new(int(16)),
        });
        assert_eq!(type_str(&fixed), "[u8; 16]");
    }

    #[test]
    fn statements_and_breaking() {
        let pat = Pattern {
            kind: PatternKind::Ident(id("x")),
            span: sp(),
        };
        let s = Stmt {
            kind: StmtKind::Let {
                kind: BindingKind::Const,
                pat,
                ty: Some(ty("i32")),
                init: Some(int(1)),
                comptime: false,
            },
            span: sp(),
        };
        assert_eq!(stmt_s(&s).to_pretty(), "(const x (: i32) 1)");

        // A long list breaks one child per line.
        let mut sum = name("aaaaaaaaaaaaaaaa");
        for _ in 0..6 {
            sum = bin(BinOp::Add, sum, name("bbbbbbbbbbbbbbbb"));
        }
        let out = dump_expr(&sum);
        assert!(out.contains('\n'), "{out}");
        assert!(out.lines().all(|l| l.len() <= WIDTH), "{out}");
    }
}
