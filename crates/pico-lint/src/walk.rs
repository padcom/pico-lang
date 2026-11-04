//! A visitor over the syntax tree: each method visits its node's children by default
//! (`walk_*`); a rule overrides the nodes it looks at and calls `walk_*` to keep going.

use pico_syntax::ast::*;

pub trait Visit<'a> {
    fn stmt(&mut self, s: &'a Stmt) {
        walk_stmt(self, s);
    }
    fn expr(&mut self, e: &'a Expr) {
        walk_expr(self, e);
    }
    fn block(&mut self, b: &'a Block) {
        walk_block(self, b);
    }
    fn decl(&mut self, d: &'a Decl) {
        walk_decl(self, d);
    }
    fn member(&mut self, m: &'a Member) {
        walk_member(self, m);
    }
    /// A function: declared, a method's, or a closure (`ExprKind::Function`).
    fn func(&mut self, f: &'a FnDecl) {
        walk_fn(self, f);
    }
    /// A pattern that binds names (`let`, parameters, `for-of`, `catch`, match arms).
    fn pattern(&mut self, _p: &'a Pattern) {}
}

pub fn walk_stmt<'a, V: Visit<'a> + ?Sized>(v: &mut V, s: &'a Stmt) {
    match &s.kind {
        StmtKind::Expr { expr, .. } => v.expr(expr),
        StmtKind::Let { pat, init, .. } => {
            v.pattern(pat);
            if let Some(i) = init {
                v.expr(i);
            }
        }
        StmtKind::Using { init, .. } => v.expr(init),
        StmtKind::While { cond, body } => {
            v.expr(cond);
            v.expr(body);
        }
        StmtKind::DoWhile { body, cond } => {
            v.block(body);
            v.expr(cond);
        }
        StmtKind::For {
            init,
            cond,
            update,
            body,
        } => {
            if let Some(i) = init {
                v.stmt(i);
            }
            if let Some(c) = cond {
                v.expr(c);
            }
            if let Some(u) = update {
                v.expr(u);
            }
            v.expr(body);
        }
        StmtKind::ForOf {
            pat, iter, body, ..
        } => {
            v.pattern(pat);
            v.expr(iter);
            v.expr(body);
        }
        StmtKind::Decl(d) => v.decl(d),
        StmtKind::Error => {}
    }
}

pub fn walk_block<'a, V: Visit<'a> + ?Sized>(v: &mut V, b: &'a Block) {
    for s in &b.stmts {
        v.stmt(s);
    }
}

pub fn walk_decl<'a, V: Visit<'a> + ?Sized>(v: &mut V, d: &'a Decl) {
    match &d.kind {
        DeclKind::Function(f) => v.func(f),
        DeclKind::Struct(s) => s.members.iter().for_each(|m| v.member(m)),
        DeclKind::Class(c) => c.members.iter().for_each(|m| v.member(m)),
        DeclKind::Interface(i) => i.members.iter().for_each(|m| v.member(m)),
        DeclKind::Enum(e) => e.members.iter().for_each(|m| v.member(m)),
        DeclKind::Binding(s) => v.stmt(s),
        DeclKind::Extern { items, .. } => items.iter().for_each(|i| v.decl(i)),
        DeclKind::TypeAlias { .. }
        | DeclKind::Import(_)
        | DeclKind::ExportList { .. }
        | DeclKind::ExportAll { .. } => {}
    }
}

pub fn walk_member<'a, V: Visit<'a> + ?Sized>(v: &mut V, m: &'a Member) {
    match &m.kind {
        MemberKind::Field { init, .. } => {
            if let Some(i) = init {
                v.expr(i);
            }
        }
        MemberKind::Method(f)
        | MemberKind::Getter(f)
        | MemberKind::Setter(f)
        | MemberKind::Constructor(f)
        | MemberKind::Operator { func: f, .. } => v.func(f),
        MemberKind::StaticBlock(b) => v.block(b),
    }
}

pub fn walk_fn<'a, V: Visit<'a> + ?Sized>(v: &mut V, f: &'a FnDecl) {
    for p in &f.params {
        v.pattern(&p.pat);
        if let Some(d) = &p.default {
            v.expr(d);
        }
    }
    match &f.body {
        Some(FnBody::Block(b)) => v.block(b),
        Some(FnBody::Expr(e)) => v.expr(e),
        None => {}
    }
}

pub fn walk_expr<'a, V: Visit<'a> + ?Sized>(v: &mut V, e: &'a Expr) {
    match &e.kind {
        ExprKind::Lit(_)
        | ExprKind::Ident(_)
        | ExprKind::This
        | ExprKind::Super
        | ExprKind::Continue
        | ExprKind::Error => {}
        ExprKind::Template { tag, exprs, .. } => {
            if let Some(t) = tag {
                v.expr(t);
            }
            for x in exprs {
                v.expr(&x.expr);
            }
        }
        ExprKind::Array(elems) => {
            for el in elems {
                match el {
                    ArrayElem::Expr(x) | ArrayElem::Spread(x) => v.expr(x),
                }
            }
        }
        ExprKind::ArrayRepeat { value, count } => {
            v.expr(value);
            v.expr(count);
        }
        ExprKind::Object { fields, .. } => {
            for f in fields {
                match f {
                    ObjField::KeyValue { value, .. } | ObjField::Spread(value) => v.expr(value),
                    ObjField::Shorthand(_) => {}
                }
            }
        }
        ExprKind::Paren(x)
        | ExprKind::NonNull(x)
        | ExprKind::TryOpt(x)
        | ExprKind::TryBang(x)
        | ExprKind::Throw(x)
        | ExprKind::Await(x)
        | ExprKind::Tailcall(x)
        | ExprKind::Comptime(x) => v.expr(x),
        ExprKind::Unary { expr, .. }
        | ExprKind::As { expr, .. }
        | ExprKind::Instanceof { expr, .. } => v.expr(expr),
        ExprKind::Is { expr, pat } => {
            v.expr(expr);
            v.pattern(pat);
        }
        ExprKind::Binary { lhs, rhs, .. } => {
            v.expr(lhs);
            v.expr(rhs);
        }
        ExprKind::Assign { target, value, .. } => {
            v.expr(target);
            v.expr(value);
        }
        ExprKind::Ternary { cond, then, else_ } => {
            v.expr(cond);
            v.expr(then);
            v.expr(else_);
        }
        ExprKind::Call { callee, args, .. } | ExprKind::New { callee, args, .. } => {
            v.expr(callee);
            for a in args {
                v.expr(&a.value);
            }
        }
        ExprKind::Member { object, .. } => v.expr(object),
        ExprKind::Index { object, index } => {
            v.expr(object);
            v.expr(index);
        }
        ExprKind::Range { start, end } => {
            v.expr(start);
            v.expr(end);
        }
        ExprKind::Function(f) => v.func(f),
        ExprKind::Block(b) | ExprKind::Loop(b) | ExprKind::Unsafe(b) | ExprKind::Arena(b) => {
            v.block(b);
        }
        ExprKind::If { cond, then, else_ } => {
            v.expr(cond);
            v.expr(then);
            if let Some(x) = else_ {
                v.expr(x);
            }
        }
        ExprKind::Match { scrutinee, arms } => {
            v.expr(scrutinee);
            for a in arms {
                v.pattern(&a.pat);
                if let Some(g) = &a.guard {
                    v.expr(g);
                }
                v.expr(&a.body);
            }
        }
        ExprKind::Switch { scrutinee, cases } => {
            v.expr(scrutinee);
            for c in cases {
                if let Some(p) = &c.pat {
                    v.pattern(p);
                }
                if let Some(g) = &c.guard {
                    v.expr(g);
                }
                v.block(&c.body);
            }
        }
        ExprKind::Try {
            body,
            catches,
            finally,
        } => {
            v.expr(body);
            for c in catches {
                v.pattern(&c.pat);
                v.expr(&c.body);
            }
            if let Some(f) = finally {
                v.block(f);
            }
        }
        ExprKind::Return(x) | ExprKind::Break(x) => {
            if let Some(x) = x {
                v.expr(x);
            }
        }
        ExprKind::Yield { expr, .. } => {
            if let Some(x) = expr {
                v.expr(x);
            }
        }
    }
}

/// The names a pattern binds (`a`, `[a, b]`, `{ x, y: py }`, `...rest`, `e: IoError`).
pub fn bound_names<'a>(p: &'a Pattern, out: &mut Vec<&'a Ident>) {
    match &p.kind {
        PatternKind::Ident(n) => out.push(n),
        PatternKind::Array(elems) | PatternKind::Or(elems) => {
            elems.iter().for_each(|e| bound_names(e, out));
        }
        PatternKind::TupleStruct { elems, .. } => elems.iter().for_each(|e| bound_names(e, out)),
        PatternKind::Rest(Some(n)) => out.push(n),
        PatternKind::Default { pat, .. } => bound_names(pat, out),
        PatternKind::Struct { fields, rest, .. } => {
            for f in fields {
                match &f.pat {
                    Some(p) => bound_names(p, out),
                    None => out.push(&f.name),
                }
            }
            if let Some(Some(n)) = rest {
                out.push(n);
            }
        }
        PatternKind::Typed { name, fields, .. } => {
            out.push(name);
            for f in fields.iter().flatten() {
                match &f.pat {
                    Some(p) => bound_names(p, out),
                    None => out.push(&f.name),
                }
            }
        }
        PatternKind::As { name, pat } => {
            out.push(name);
            bound_names(pat, out);
        }
        _ => {}
    }
}
