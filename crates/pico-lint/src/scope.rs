//! Name resolution on the syntax tree, for the rules that ask where a name is declared, read
//! and written (`shadow`, `preferConst`, `loopFunc`, `underscoreDangle`).
//!
//! Each block, function, loop, `catch` and match arm is a scope; a block's `const`/`let`,
//! functions and types are declared when the block is entered (using a binding before its
//! declaration is a compile error, so that changes nothing). Inside a type body the members
//! are a scope of their own, so implicit members (`count` for `this.count`) resolve to them
//! and not to a module-level binding. A pattern name that resolves to something in scope is a
//! constant or variant being matched, not a new binding.

use crate::walk::{self, Visit, bound_names};
use pico_diag::Span;
use pico_syntax::ast::*;
use std::collections::HashMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Const,
    Let,
    /// A `let` declared in a `for (…; …; …)` header (a fresh one per iteration).
    ForLet,
    Param,
    Function,
    Type,
    Import,
    /// `catch`, `for-of`, match arms, `is` and `using` bindings.
    Pattern,
    Member,
}

#[derive(Debug)]
pub struct Binding {
    pub name: String,
    pub span: Span,
    pub kind: Kind,
    /// Bindings declared by the same statement share a group (`let [a, b] = …`).
    pub group: usize,
    /// The `let` has an initializer (or is a `for-of` binding).
    pub init: bool,
    pub reads: u32,
    pub writes: u32,
    /// Exempt from the rules: `comptime`, `reactive`, `export`ed bindings.
    pub special: bool,
}

/// A closure inside a loop: the closure's span and the number of bindings that existed when
/// the innermost loop around it was entered.
#[derive(Clone, Copy, Debug)]
pub struct LoopClosure {
    pub span: Span,
    pub before: usize,
}

#[derive(Debug)]
pub struct Ref {
    pub binding: usize,
    pub closure: Option<LoopClosure>,
}

#[derive(Default, Debug)]
pub struct Scopes {
    pub bindings: Vec<Binding>,
    pub refs: Vec<Ref>,
    /// (inner, outer): a declaration hiding one of an enclosing scope.
    pub shadows: Vec<(usize, usize)>,
}

pub fn resolve(m: &Module) -> Scopes {
    let mut r = Resolver::default();
    r.loops.push(Vec::new());
    r.push();
    r.declare_stmts(&m.stmts);
    for s in &m.stmts {
        r.stmt(s);
    }
    r.out
}

#[derive(Default)]
struct Frame {
    names: HashMap<String, usize>,
}

#[derive(Default)]
struct Resolver {
    out: Scopes,
    frames: Vec<Frame>,
    /// Loops of the current function: the binding count when each was entered.
    loops: Vec<Vec<usize>>,
    closure: Option<LoopClosure>,
    groups: usize,
}

impl Resolver {
    fn push(&mut self) {
        self.frames.push(Frame::default());
    }

    fn pop(&mut self) {
        self.frames.pop();
    }

    fn lookup(&self, name: &str) -> Option<usize> {
        self.frames
            .iter()
            .rev()
            .find_map(|f| f.names.get(name).copied())
    }

    fn declare(&mut self, id: &Ident, kind: Kind, group: usize, init: bool, special: bool) {
        if id.name == "_" || id.name.is_empty() {
            return;
        }
        let top = self.frames.len() - 1;
        if self.frames[top].names.contains_key(&id.name) {
            return; // an overload or a redeclaration (a compile error)
        }
        let n = self.out.bindings.len();
        let outer = self.frames[..top]
            .iter()
            .rev()
            .find_map(|f| f.names.get(&id.name).copied());
        if let Some(o) = outer {
            let ob = &self.out.bindings[o];
            let earlier = matches!(ob.kind, Kind::Function | Kind::Type | Kind::Import)
                || ob.span.start < id.span.start;
            if ob.kind != Kind::Member && earlier && kind != Kind::Member {
                self.out.shadows.push((n, o));
            }
        }
        self.out.bindings.push(Binding {
            name: id.name.clone(),
            span: id.span,
            kind,
            group,
            init,
            reads: 0,
            writes: 0,
            special,
        });
        self.frames[top].names.insert(id.name.clone(), n);
    }

    fn group(&mut self) -> usize {
        self.groups += 1;
        self.groups
    }

    fn declare_pattern(&mut self, p: &Pattern, kind: Kind, init: bool, special: bool) {
        let g = self.group();
        let mut names = Vec::new();
        bound_names(p, &mut names);
        for n in names {
            self.declare(n, kind, g, init, special);
        }
        self.pattern_defaults(p);
    }

    /// The default values inside a pattern (`{ port = 8080 }`) are expressions.
    fn pattern_defaults(&mut self, p: &Pattern) {
        match &p.kind {
            PatternKind::Default { pat, default } => {
                self.pattern_defaults(pat);
                self.expr(default);
            }
            PatternKind::Array(ps) | PatternKind::Or(ps) => {
                ps.iter().for_each(|p| self.pattern_defaults(p))
            }
            PatternKind::TupleStruct { elems, .. } => {
                elems.iter().for_each(|p| self.pattern_defaults(p))
            }
            PatternKind::Struct { fields, .. } => fields
                .iter()
                .filter_map(|f| f.pat.as_ref())
                .for_each(|p| self.pattern_defaults(p)),
            PatternKind::As { pat, .. } => self.pattern_defaults(pat),
            _ => {}
        }
    }

    /// A pattern that matches a value: names already in scope are read, not declared.
    fn match_pattern(&mut self, p: &Pattern) {
        let g = self.group();
        let mut names = Vec::new();
        bound_names(p, &mut names);
        for n in names {
            match self.lookup(&n.name) {
                Some(b) if matches!(p.kind, PatternKind::Ident(_)) => self.read(b),
                _ => self.declare(n, Kind::Pattern, g, true, false),
            }
        }
    }

    fn declare_stmts(&mut self, stmts: &[Stmt]) {
        for s in stmts {
            match &s.kind {
                StmtKind::Let { .. } => self.declare_let(s, false),
                StmtKind::Decl(d) => self.declare_decl(d),
                _ => {}
            }
        }
    }

    fn declare_let(&mut self, s: &Stmt, exported: bool) {
        if let StmtKind::Let {
            kind,
            pat,
            init,
            comptime,
            ..
        } = &s.kind
        {
            let k = match kind {
                BindingKind::Let => Kind::Let,
                BindingKind::Const | BindingKind::Reactive => Kind::Const,
            };
            let special = *comptime || exported || *kind == BindingKind::Reactive;
            let g = self.group();
            let mut names = Vec::new();
            bound_names(pat, &mut names);
            for n in names {
                self.declare(n, k, g, init.is_some(), special);
            }
        }
    }

    fn declare_decl(&mut self, d: &Decl) {
        let g = self.group();
        match &d.kind {
            DeclKind::Function(f) => {
                if let Some(n) = &f.name {
                    self.declare(n, Kind::Function, g, true, d.exported);
                }
            }
            DeclKind::Struct(s) => self.declare(&s.name, Kind::Type, g, true, true),
            DeclKind::Class(c) => self.declare(&c.name, Kind::Type, g, true, true),
            DeclKind::Enum(e) => self.declare(&e.name, Kind::Type, g, true, true),
            DeclKind::Interface(i) => self.declare(&i.name, Kind::Type, g, true, true),
            DeclKind::TypeAlias { name, .. } => self.declare(name, Kind::Type, g, true, true),
            DeclKind::Import(i) => match &i.kind {
                ImportKind::Named(names) => {
                    for n in names {
                        self.declare(
                            n.alias.as_ref().unwrap_or(&n.name),
                            Kind::Import,
                            g,
                            true,
                            true,
                        );
                    }
                }
                ImportKind::Namespace(n) => self.declare(n, Kind::Import, g, true, true),
            },
            DeclKind::Binding(s) => self.declare_let(s, true),
            DeclKind::Extern { items, .. } => items.iter().for_each(|i| self.declare_decl(i)),
            DeclKind::ExportList { .. } | DeclKind::ExportAll { .. } => {}
        }
    }

    fn read(&mut self, b: usize) {
        self.out.bindings[b].reads += 1;
        self.out.refs.push(Ref {
            binding: b,
            closure: self.closure,
        });
    }

    fn write(&mut self, target: &Expr, also_read: bool) {
        let target = strip_parens(target);
        if let ExprKind::Ident(name) = &target.kind {
            if let Some(b) = self.lookup(name) {
                self.out.bindings[b].writes += 1;
                if also_read {
                    self.out.bindings[b].reads += 1;
                }
                self.out.refs.push(Ref {
                    binding: b,
                    closure: self.closure,
                });
            }
        } else {
            self.expr(target);
        }
    }

    fn enter_loop(&mut self) {
        let n = self.out.bindings.len();
        if let Some(l) = self.loops.last_mut() {
            l.push(n);
        }
    }

    fn leave_loop(&mut self) {
        if let Some(l) = self.loops.last_mut() {
            l.pop();
        }
    }

    fn members(&mut self, members: &[Member]) {
        self.push();
        let g = self.group();
        for m in members {
            let name = match &m.kind {
                MemberKind::Field { name, .. } => Some(name),
                MemberKind::Method(f) | MemberKind::Getter(f) | MemberKind::Setter(f) => {
                    f.name.as_ref()
                }
                _ => None,
            };
            if let Some(n) = name {
                self.declare(n, Kind::Member, g, true, true);
            }
            // Constructor parameter properties are fields too.
            if let MemberKind::Constructor(f) = &m.kind {
                for p in f.params.iter().filter(|p| !p.mods.is_empty()) {
                    if let PatternKind::Ident(n) = &p.pat.kind {
                        self.declare(n, Kind::Member, g, true, true);
                    }
                }
            }
        }
        for m in members {
            self.member(m);
        }
        self.pop();
    }
}

pub fn strip_parens(e: &Expr) -> &Expr {
    match &e.kind {
        ExprKind::Paren(x) => strip_parens(x),
        _ => e,
    }
}

impl<'a> Visit<'a> for Resolver {
    fn block(&mut self, b: &'a Block) {
        self.push();
        self.declare_stmts(&b.stmts);
        walk::walk_block(self, b);
        self.pop();
    }

    fn decl(&mut self, d: &'a Decl) {
        match &d.kind {
            DeclKind::Struct(s) => self.members(&s.members),
            DeclKind::Class(c) => self.members(&c.members),
            DeclKind::Interface(i) => self.members(&i.members),
            DeclKind::Enum(e) => self.members(&e.members),
            _ => walk::walk_decl(self, d),
        }
    }

    fn func(&mut self, f: &'a FnDecl) {
        let saved = self.closure;
        // A closure inside a loop of the function around it.
        if let Some(&before) = self.loops.last().and_then(|l| l.last())
            && self.closure.is_none()
        {
            self.closure = Some(LoopClosure {
                span: f.span,
                before,
            });
        }
        self.loops.push(Vec::new());
        self.push();
        let g = self.group();
        for p in &f.params {
            let mut names = Vec::new();
            bound_names(&p.pat, &mut names);
            for n in names {
                // Parameter properties are members (declared with the type body).
                if p.mods.is_empty() {
                    self.declare(n, Kind::Param, g, true, false);
                }
            }
            self.pattern_defaults(&p.pat);
            if let Some(d) = &p.default {
                self.expr(d);
            }
        }
        match &f.body {
            Some(FnBody::Block(b)) => self.block(b),
            Some(FnBody::Expr(e)) => self.expr(e),
            None => {}
        }
        self.pop();
        self.loops.pop();
        self.closure = saved;
    }

    fn stmt(&mut self, s: &'a Stmt) {
        match &s.kind {
            // Declared when the block was entered.
            StmtKind::Let { pat, init, .. } => {
                self.pattern_defaults(pat);
                if let Some(i) = init {
                    self.expr(i);
                }
            }
            StmtKind::Using { name, init, .. } => {
                self.expr(init);
                let g = self.group();
                self.declare(name, Kind::Pattern, g, true, false);
            }
            StmtKind::For {
                init,
                cond,
                update,
                body,
            } => {
                self.enter_loop();
                self.push();
                if let Some(i) = init {
                    if let StmtKind::Let {
                        kind: BindingKind::Let,
                        pat,
                        ..
                    } = &i.kind
                    {
                        let g = self.group();
                        let mut names = Vec::new();
                        bound_names(pat, &mut names);
                        for n in names {
                            self.declare(n, Kind::ForLet, g, true, false);
                        }
                    } else {
                        self.declare_stmts(std::slice::from_ref(i));
                    }
                    self.stmt(i);
                }
                if let Some(c) = cond {
                    self.expr(c);
                }
                if let Some(u) = update {
                    self.expr(u);
                }
                self.expr(body);
                self.pop();
                self.leave_loop();
            }
            StmtKind::ForOf {
                kind,
                pat,
                iter,
                body,
                ..
            } => {
                self.expr(iter);
                self.enter_loop();
                self.push();
                let k = if *kind == BindingKind::Let {
                    Kind::Let
                } else {
                    Kind::Pattern
                };
                self.declare_pattern(pat, k, true, false);
                self.expr(body);
                self.pop();
                self.leave_loop();
            }
            StmtKind::While { cond, body } => {
                self.enter_loop();
                self.expr(cond);
                self.expr(body);
                self.leave_loop();
            }
            StmtKind::DoWhile { body, cond } => {
                self.enter_loop();
                self.block(body);
                self.expr(cond);
                self.leave_loop();
            }
            _ => walk::walk_stmt(self, s),
        }
    }

    fn expr(&mut self, e: &'a Expr) {
        match &e.kind {
            ExprKind::Ident(name) => {
                if let Some(b) = self.lookup(name) {
                    self.read(b);
                }
            }
            ExprKind::Object { fields, .. } => {
                for f in fields {
                    match f {
                        ObjField::Shorthand(n) => {
                            if let Some(b) = self.lookup(&n.name) {
                                self.read(b);
                            }
                        }
                        ObjField::KeyValue { value, .. } | ObjField::Spread(value) => {
                            self.expr(value)
                        }
                        ObjField::Computed { key, value } => {
                            self.expr(key);
                            self.expr(value);
                        }
                    }
                }
            }
            ExprKind::Assign { op, target, value } => {
                self.expr(value);
                self.write(target, op.is_some());
            }
            ExprKind::Unary {
                op: UnOp::PreInc | UnOp::PreDec | UnOp::PostInc | UnOp::PostDec,
                expr,
            } => self.write(expr, true),
            ExprKind::Loop(b) => {
                self.enter_loop();
                self.block(b);
                self.leave_loop();
            }
            ExprKind::Is { expr, pat } => {
                self.expr(expr);
                self.match_pattern(pat);
            }
            ExprKind::Match { scrutinee, arms } => {
                self.expr(scrutinee);
                for a in arms {
                    self.push();
                    self.match_pattern(&a.pat);
                    if let Some(g) = &a.guard {
                        self.expr(g);
                    }
                    self.expr(&a.body);
                    self.pop();
                }
            }
            ExprKind::Switch { scrutinee, cases } => {
                self.expr(scrutinee);
                for c in cases {
                    self.push();
                    if let Some(p) = &c.pat {
                        self.match_pattern(p);
                    }
                    if let Some(g) = &c.guard {
                        self.expr(g);
                    }
                    self.block(&c.body);
                    self.pop();
                }
            }
            ExprKind::Try {
                body,
                catches,
                finally,
            } => {
                self.expr(body);
                for c in catches {
                    self.push();
                    self.declare_pattern(&c.pat, Kind::Pattern, true, false);
                    self.expr(&c.body);
                    self.pop();
                }
                if let Some(f) = finally {
                    self.block(f);
                }
            }
            _ => walk::walk_expr(self, e),
        }
    }
}
