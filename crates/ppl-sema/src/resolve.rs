//! Name resolution (spec §4.1–§4.2, single module): AST → HIR.
//!
//! - Block scoping, no hoisting: using a binding before its declaration is an error, including
//!   inside its own initializer and when an inner block declares the name later (as JS's TDZ).
//! - Top-level `function` declarations are visible everywhere in the module.
//! - The entry module's top-level bindings are globals. Top-level code sees them in order;
//!   function bodies see all of them (they run later, like closures over the script scope).
//! - `print`, `println`, `assert` and `panic` are built-ins.
//!
//! Constructs that parse but aren't implemented yet report `E0900`.

use crate::hir::*;
use crate::ty::Ty;
use ppl_diag::{Code, Diagnostic, FileId, Span, codes};
use ppl_syntax::ast;
use std::collections::{HashMap, HashSet};

pub fn resolve(module: &ast::Module, file: FileId) -> (Program, Vec<Diagnostic>) {
    let mut r = Resolver {
        file,
        diags: Vec::new(),
        fns: Vec::new(),
        globals: Vec::new(),
        spans: Vec::new(),
        top_fns: HashMap::new(),
        top_globals: HashMap::new(),
        globals_declared: HashSet::new(),
        f: FnState::default(),
        structs: Vec::new(),
        top_structs: HashMap::new(),
    };
    let program = r.program(module);
    (program, r.diags)
}

#[derive(Default)]
struct Scope {
    names: HashMap<String, LocalId>,
    /// Bindings this block declares further down, for use-before-declaration errors.
    later: HashMap<String, Span>,
}

#[derive(Clone, Copy, PartialEq)]
enum LoopKind {
    /// `loop { }`: `break value` allowed.
    Loop,
    /// `while`, `do`, `for`.
    While,
}

/// Per-function state.
#[derive(Default)]
struct FnState {
    locals: Vec<Binding>,
    scopes: Vec<Scope>,
    loops: Vec<LoopKind>,
    in_entry: bool,
    /// Inside a method: the `this` local and its struct.
    this: Option<(LocalId, StructId)>,
}

struct Resolver {
    file: FileId,
    diags: Vec<Diagnostic>,
    fns: Vec<Option<Function>>,
    globals: Vec<Binding>,
    spans: Vec<Span>,
    top_fns: HashMap<String, FnId>,
    top_globals: HashMap<String, GlobalId>,
    globals_declared: HashSet<GlobalId>,
    f: FnState,
    structs: Vec<StructDef>,
    top_structs: HashMap<String, StructId>,
}

enum Found {
    Var(Var),
    Fn(FnId),
    Builtin(Builtin),
    /// A field or method of `this` (implicit member).
    Member(String),
}

impl Resolver {
    fn error(&mut self, code: Code, span: Span, msg: impl Into<String>, label: impl Into<String>) {
        self.diags
            .push(Diagnostic::error(code, msg).primary(span, label));
    }

    fn not_implemented(&mut self, span: Span, what: &str) {
        self.error(
            codes::NOT_IMPLEMENTED,
            span,
            format!("{what} isn't implemented yet"),
            "not supported by this compiler version",
        );
    }

    fn expr(&mut self, kind: ExprKind, span: Span) -> Expr {
        let id = ExprId(self.spans.len() as u32);
        self.spans.push(span);
        Expr { id, kind, span }
    }

    fn error_expr(&mut self, span: Span) -> Expr {
        self.expr(ExprKind::Error, span)
    }

    // ------------------------------------------------------------ program

    fn program(&mut self, module: &ast::Module) -> Program {
        // Pass 1: top-level functions and bindings.
        let mut fn_decls: Vec<(FnId, &ast::FnDecl, Option<StructId>)> = Vec::new();
        let mut struct_decls: Vec<(StructId, &ast::StructDecl)> = Vec::new();
        let mut first_seen: HashMap<String, Span> = HashMap::new();
        for stmt in &module.stmts {
            match &stmt.kind {
                ast::StmtKind::Decl(d) => {
                    if let ast::DeclKind::Struct(sd) = &d.kind {
                        if let Some(dec) = d.decorators.first() {
                            self.not_implemented(dec.span, "decorators");
                        }
                        if self.check_duplicate(&mut first_seen, &sd.name) {
                            continue;
                        }
                        let id = StructId(self.structs.len() as u32);
                        self.structs.push(StructDef {
                            name: sd.name.name.clone(),
                            name_span: sd.name.span,
                            fields: Vec::new(),
                            methods: Vec::new(),
                        });
                        self.top_structs.insert(sd.name.name.clone(), id);
                        struct_decls.push((id, sd));
                    }
                    if let ast::DeclKind::Function(f) = &d.kind
                        && let Some(name) = &f.name
                    {
                        if !d.decorators.is_empty() {
                            self.not_implemented(d.decorators[0].span, "decorators");
                        }
                        if self.check_duplicate(&mut first_seen, name) {
                            continue;
                        }
                        let id = FnId(self.fns.len() as u32);
                        self.fns.push(None);
                        self.top_fns.insert(name.name.clone(), id);
                        fn_decls.push((id, f, None));
                    }
                }
                ast::StmtKind::Let {
                    kind,
                    pat,
                    ty,
                    comptime,
                    ..
                } => {
                    if *comptime {
                        continue;
                    }
                    if let ast::PatternKind::Ident(name) = &pat.kind {
                        if self.check_duplicate(&mut first_seen, name) {
                            continue;
                        }
                        let ty = ty.as_ref().map(|t| self.resolve_type(t));
                        let id = GlobalId(self.globals.len() as u32);
                        self.globals.push(Binding {
                            name: name.name.clone(),
                            mutable: *kind == ast::BindingKind::Let,
                            ty,
                            span: name.span,
                        });
                        self.top_globals.insert(name.name.clone(), id);
                    }
                }
                _ => {}
            }
        }

        // Pass 1b: struct fields and methods (all struct names are known now).
        for (sid, sd) in &struct_decls {
            self.struct_members(*sid, sd, &mut fn_decls);
        }
        self.check_struct_cycles();

        // Pass 2: function and method bodies.
        for (id, f, this) in fn_decls {
            let func = self.function(f, false, this);
            self.fns[id.0 as usize] = Some(func);
        }

        // Pass 3: the entry body (top-level statements in order).
        let entry_id = FnId(self.fns.len() as u32);
        self.f = FnState {
            in_entry: true,
            ..Default::default()
        };
        self.f.scopes.push(Scope::default());
        let block = self.top_level(&module.stmts, module.span);
        let locals = std::mem::take(&mut self.f.locals);
        self.fns.push(Some(Function {
            name: "<main>".into(),
            name_span: module.span.shrink_to_lo(),
            params: Vec::new(),
            ret: Some(Ty::Void),
            ret_span: None,
            locals,
            body: block,
            span: module.span,
            is_entry: true,
            this: None,
        }));

        Program {
            file: self.file,
            fns: self
                .fns
                .drain(..)
                .map(|f| f.expect("every function resolved"))
                .collect(),
            entry: entry_id,
            globals: std::mem::take(&mut self.globals),
            expr_spans: std::mem::take(&mut self.spans),
            structs: std::mem::take(&mut self.structs),
        }
    }

    fn struct_members<'a>(
        &mut self,
        sid: StructId,
        sd: &'a ast::StructDecl,
        fn_decls: &mut Vec<(FnId, &'a ast::FnDecl, Option<StructId>)>,
    ) {
        if let Some(g) = sd.generics.first() {
            self.not_implemented(g.span, "generic structs");
        }
        if let Some(t) = sd.implements.first() {
            self.not_implemented(t.span, "interfaces");
        }
        let mut seen: HashMap<String, Span> = HashMap::new();
        for m in &sd.members {
            if let Some(dec) = m.decorators.first() {
                self.not_implemented(dec.span, "decorators");
            }
            if !m.mods.is_empty() {
                self.not_implemented(m.span, "member modifiers");
            }
            match &m.kind {
                ast::MemberKind::Field {
                    name,
                    ty,
                    optional,
                    init,
                } => {
                    if self.check_duplicate(&mut seen, name) {
                        continue;
                    }
                    if *optional || init.is_some() {
                        self.not_implemented(m.span, "optional fields and field defaults");
                    }
                    let ty = match ty {
                        Some(t) => self.resolve_type(t),
                        None => {
                            self.error(
                                codes::MISSING_PARAM_TYPE,
                                name.span,
                                format!("field `{}` needs a type", name.name),
                                "add a type annotation, e.g. `: f32`",
                            );
                            Ty::Error
                        }
                    };
                    self.structs[sid.0 as usize].fields.push(FieldDef {
                        name: name.name.clone(),
                        ty,
                        span: name.span,
                    });
                }
                ast::MemberKind::Method(f) => {
                    let Some(name) = &f.name else { continue };
                    if self.check_duplicate(&mut seen, name) {
                        continue;
                    }
                    let id = FnId(self.fns.len() as u32);
                    self.fns.push(None);
                    self.structs[sid.0 as usize]
                        .methods
                        .push((name.name.clone(), id));
                    fn_decls.push((id, f, Some(sid)));
                }
                ast::MemberKind::Operator { .. } => {
                    self.not_implemented(m.span, "operator overloading")
                }
                ast::MemberKind::Getter(_) | ast::MemberKind::Setter(_) => {
                    self.not_implemented(m.span, "getters and setters")
                }
                ast::MemberKind::Constructor(_) | ast::MemberKind::StaticBlock(_) => {
                    self.not_implemented(m.span, "constructors and static blocks in structs")
                }
            }
        }
    }

    /// A struct that contains itself by value (directly or through other structs) has no size.
    fn check_struct_cycles(&mut self) {
        fn visit(r: &Resolver, id: StructId, path: &mut Vec<StructId>) -> Option<Vec<StructId>> {
            if let Some(pos) = path.iter().position(|p| *p == id) {
                return Some(path[pos..].to_vec());
            }
            path.push(id);
            for f in &r.structs[id.0 as usize].fields {
                if let Ty::Struct(inner, _) = &f.ty
                    && let Some(c) = visit(r, *inner, path)
                {
                    return Some(c);
                }
            }
            path.pop();
            None
        }
        let mut reported: HashSet<StructId> = HashSet::new();
        for i in 0..self.structs.len() {
            let id = StructId(i as u32);
            if reported.contains(&id) {
                continue;
            }
            if let Some(cycle) = visit(self, id, &mut Vec::new())
                && cycle[0] == id
            {
                reported.extend(cycle.iter().copied());
                let names: Vec<&str> = cycle
                    .iter()
                    .map(|c| self.structs[c.0 as usize].name.as_str())
                    .collect();
                let d = Diagnostic::error(
                    codes::RECURSIVE_STRUCT,
                    format!("struct `{}` contains itself", names[0]),
                )
                .primary(
                    self.structs[i].name_span,
                    "this struct would have infinite size",
                )
                .note(format!("{} → {}", names.join(" → "), names[0]))
                .help("structs are stored inline; use a class for recursive data");
                self.diags.push(d);
            }
        }
    }

    /// Reports a duplicate top-level name. Returns true if `name` was already declared.
    fn check_duplicate(&mut self, seen: &mut HashMap<String, Span>, name: &ast::Ident) -> bool {
        if let Some(&prev) = seen.get(&name.name) {
            self.diags.push(
                Diagnostic::error(
                    codes::DUPLICATE_DECL,
                    format!("`{}` is declared twice", name.name),
                )
                .primary(name.span, "declared again here")
                .secondary(prev, "first declared here"),
            );
            return true;
        }
        seen.insert(name.name.clone(), name.span);
        false
    }

    fn top_level(&mut self, stmts: &[ast::Stmt], span: Span) -> Block {
        let mut out = Vec::new();
        let mut tail = None;
        for (i, stmt) in stmts.iter().enumerate() {
            match &stmt.kind {
                ast::StmtKind::Let {
                    pat,
                    init,
                    comptime: false,
                    ..
                } => {
                    let ast::PatternKind::Ident(name) = &pat.kind else {
                        self.not_implemented(pat.span, "destructuring");
                        continue;
                    };
                    let init = init.as_ref().map(|e| self.lower_expr(e));
                    match self.top_globals.get(&name.name).copied() {
                        // Only the first declaration of a duplicated name became a global.
                        Some(g) if self.globals[g.0 as usize].span == name.span => {
                            self.globals_declared.insert(g);
                            out.push(Stmt::Let {
                                var: Var::Global(g),
                                init,
                                span: stmt.span,
                            });
                        }
                        _ => {
                            if let Some(e) = init {
                                out.push(Stmt::Expr(e));
                            }
                        }
                    }
                }
                ast::StmtKind::Decl(d)
                    if matches!(
                        d.kind,
                        ast::DeclKind::Function(_) | ast::DeclKind::Struct(_)
                    ) => {}
                _ => {
                    let last = i + 1 == stmts.len();
                    self.lower_stmt(stmt, &mut out, if last { Some(&mut tail) } else { None });
                }
            }
        }
        Block {
            stmts: out,
            tail,
            span,
        }
    }

    fn function(
        &mut self,
        f: &ast::FnDecl,
        is_entry: bool,
        this_struct: Option<StructId>,
    ) -> Function {
        let name = f.name.clone().expect("named function");
        if f.flags.is_async || f.flags.generator || f.flags.is_unsafe || f.flags.decorator {
            let what = if f.flags.is_async {
                "async functions"
            } else if f.flags.generator {
                "generators"
            } else if f.flags.is_unsafe {
                "unsafe functions"
            } else {
                "decorator functions"
            };
            self.not_implemented(name.span, what);
        }
        if !f.generics.is_empty() {
            self.not_implemented(f.generics[0].span, "generic functions");
        }
        if let Some(t) = &f.throws {
            self.not_implemented(t.span, "`throws` clauses");
        }
        self.f = FnState {
            in_entry: is_entry,
            ..Default::default()
        };
        self.f.scopes.push(Scope::default());
        if let Some(sid) = this_struct {
            let ty = self.struct_ty(sid);
            let id = LocalId(self.f.locals.len() as u32);
            self.f.locals.push(Binding {
                name: "this".into(),
                mutable: false,
                ty: Some(ty),
                span: name.span,
            });
            self.f.this = Some((id, sid));
        }
        let mut params = Vec::new();
        for p in &f.params {
            let ast::PatternKind::Ident(pname) = &p.pat.kind else {
                self.not_implemented(p.pat.span, "destructured parameters");
                continue;
            };
            if p.rest || p.optional || p.default.is_some() || p.comptime || !p.mods.is_empty() {
                self.not_implemented(p.span, "optional, default, rest and comptime parameters");
            }
            let ty = match &p.ty {
                Some(t) => Some(self.resolve_type(t)),
                None => {
                    self.diags.push(
                        Diagnostic::error(
                            codes::MISSING_PARAM_TYPE,
                            format!("parameter `{}` needs a type", pname.name),
                        )
                        .primary(pname.span, "add a type annotation, e.g. `: i32`")
                        .note("function parameters always declare their types (spec §5.1)"),
                    );
                    Some(Ty::Error)
                }
            };
            if self.f.scopes[0].names.contains_key(&pname.name) {
                self.error(
                    codes::DUPLICATE_DECL,
                    pname.span,
                    format!("parameter `{}` is declared twice", pname.name),
                    "duplicate parameter",
                );
                continue;
            }
            let id = self.declare_local(pname, true, ty);
            params.push(id);
        }
        let ret = f.ret.as_ref().map(|t| self.resolve_type(t));
        let body = match &f.body {
            Some(ast::FnBody::Block(b)) => self.block(b),
            Some(ast::FnBody::Expr(e)) => {
                let tail = self.lower_expr(e);
                Block {
                    stmts: Vec::new(),
                    tail: Some(Box::new(tail)),
                    span: e.span,
                }
            }
            None => {
                self.not_implemented(name.span, "functions without a body");
                Block {
                    stmts: Vec::new(),
                    tail: None,
                    span: f.span,
                }
            }
        };
        let full_name = match this_struct {
            Some(sid) => format!("{}.{}", self.structs[sid.0 as usize].name, name.name),
            None => name.name.clone(),
        };
        Function {
            name: full_name,
            name_span: name.span,
            params,
            ret,
            ret_span: f.ret.as_ref().map(|t| t.span),
            locals: std::mem::take(&mut self.f.locals),
            body,
            span: f.span,
            is_entry,
            this: self.f.this,
        }
    }

    fn struct_ty(&self, id: StructId) -> Ty {
        Ty::Struct(id, self.structs[id.0 as usize].name.as_str().into())
    }

    // ------------------------------------------------------------ scopes

    fn declare_local(&mut self, name: &ast::Ident, mutable: bool, ty: Option<Ty>) -> LocalId {
        let id = LocalId(self.f.locals.len() as u32);
        self.f.locals.push(Binding {
            name: name.name.clone(),
            mutable,
            ty,
            span: name.span,
        });
        let scope = self.f.scopes.last_mut().expect("a scope");
        scope.later.remove(&name.name);
        scope.names.insert(name.name.clone(), id);
        id
    }

    /// A compiler-generated local (loop counters).
    fn hidden_local(&mut self, name: &str, span: Span) -> LocalId {
        let id = LocalId(self.f.locals.len() as u32);
        self.f.locals.push(Binding {
            name: name.into(),
            mutable: true,
            ty: None,
            span,
        });
        id
    }

    fn lookup(&mut self, name: &str, span: Span) -> Option<Found> {
        for scope in self.f.scopes.iter().rev() {
            if let Some(&id) = scope.names.get(name) {
                return Some(Found::Var(Var::Local(id)));
            }
            if let Some(&decl) = scope.later.get(name) {
                self.use_before_decl(name, span, decl);
                return None;
            }
        }
        // Inside a method, `name` means `this.name` unless a local shadows it.
        if let Some((_, sid)) = self.f.this {
            let def = &self.structs[sid.0 as usize];
            if def.field(name).is_some() || def.method(name).is_some() {
                return Some(Found::Member(name.to_string()));
            }
        }
        if let Some(&g) = self.top_globals.get(name) {
            if self.f.in_entry && !self.globals_declared.contains(&g) {
                let decl = self.globals[g.0 as usize].span;
                self.use_before_decl(name, span, decl);
                return None;
            }
            return Some(Found::Var(Var::Global(g)));
        }
        if let Some(&f) = self.top_fns.get(name) {
            return Some(Found::Fn(f));
        }
        if let Some(b) = Builtin::from_name(name) {
            return Some(Found::Builtin(b));
        }
        if Ty::from_name(name).is_some() {
            self.error(
                codes::NOT_A_VALUE,
                span,
                format!("`{name}` is a type, not a value"),
                "type used as a value",
            );
            return None;
        }
        let mut d = Diagnostic::error(
            codes::UNKNOWN_NAME,
            format!("cannot find `{name}` in this scope"),
        )
        .primary(span, "not found");
        if let Some(s) = self.suggest(name) {
            d = d.help(format!("a name with a similar spelling exists: `{s}`"));
        }
        self.diags.push(d);
        None
    }

    fn use_before_decl(&mut self, name: &str, span: Span, decl: Span) {
        self.diags.push(
            Diagnostic::error(
                codes::USE_BEFORE_DECL,
                format!("`{name}` is used before its declaration"),
            )
            .primary(span, "used here")
            .secondary(decl, "declared here")
            .note("bindings aren't hoisted: declare them before using them"),
        );
    }

    fn suggest(&self, name: &str) -> Option<String> {
        let mut candidates: Vec<&str> = Vec::new();
        for scope in &self.f.scopes {
            candidates.extend(scope.names.keys().map(String::as_str));
        }
        candidates.extend(self.top_globals.keys().map(String::as_str));
        candidates.extend(self.top_fns.keys().map(String::as_str));
        candidates.extend(["print", "println", "assert", "panic"]);
        let max = (name.len() / 3).max(1);
        candidates
            .into_iter()
            .filter(|c| *c != name)
            .map(|c| (levenshtein(name, c), c))
            .filter(|(d, _)| *d <= max)
            .min()
            .map(|(_, c)| c.to_string())
    }

    /// The target of an assignment to a bare name (a variable, or an implicit member).
    fn resolve_target(&mut self, name: &str, span: Span) -> Option<Target> {
        if let Some(Found::Member(field)) = self.peek_member(name) {
            let (this, _) = self.f.this.expect("member lookup implies a method");
            let object = self.expr(ExprKind::Var(Var::Local(this)), span);
            let id = self.expr(ExprKind::Error, span).id;
            return Some(Target::Field {
                id,
                object: Box::new(object),
                name: field,
                name_span: span,
            });
        }
        let var = self.resolve_var(name, span)?;
        self.check_mutable(var, span);
        Some(Target::Var(var))
    }

    /// Whether `name` resolves to an implicit member (without reporting anything).
    fn peek_member(&self, name: &str) -> Option<Found> {
        if self
            .f
            .scopes
            .iter()
            .any(|s| s.names.contains_key(name) || s.later.contains_key(name))
        {
            return None;
        }
        let (_, sid) = self.f.this?;
        let def = &self.structs[sid.0 as usize];
        def.field(name)
            .is_some()
            .then(|| Found::Member(name.to_string()))
    }

    fn resolve_var(&mut self, name: &str, span: Span) -> Option<Var> {
        match self.lookup(name, span)? {
            Found::Var(v) => Some(v),
            Found::Fn(_) | Found::Builtin(_) | Found::Member(_) => {
                self.error(
                    codes::NOT_A_VALUE,
                    span,
                    format!("can't assign to the function `{name}`"),
                    "not a variable",
                );
                None
            }
        }
    }

    fn check_mutable(&mut self, var: Var, span: Span) {
        let b = match var {
            Var::Local(l) => &self.f.locals[l.0 as usize],
            Var::Global(g) => &self.globals[g.0 as usize],
        };
        if !b.mutable {
            let (name, decl) = (b.name.clone(), b.span);
            self.diags.push(
                Diagnostic::error(codes::ASSIGN_TO_CONST, format!("cannot assign to `{name}`"))
                    .primary(span, "assignment to a constant")
                    .secondary(decl, "declared with `const` here")
                    .help(format!(
                        "declare it with `let` to reassign it: `let {name} = …`"
                    )),
            );
        }
    }

    // ------------------------------------------------------------ types

    pub(crate) fn resolve_type(&mut self, t: &ast::Type) -> Ty {
        match &t.kind {
            ast::TypeKind::Path { path, args } if path.len() == 1 && args.is_empty() => {
                match Ty::from_name(&path[0].name) {
                    Some(ty) => ty,
                    None if self.top_structs.contains_key(&path[0].name) => {
                        self.struct_ty(self.top_structs[&path[0].name])
                    }
                    None => {
                        self.error(
                            codes::UNKNOWN_TYPE,
                            t.span,
                            format!("cannot find type `{}`", path[0].name),
                            "unknown type",
                        );
                        Ty::Error
                    }
                }
            }
            ast::TypeKind::Nullable(inner) => self.resolve_type(inner).nullable(),
            ast::TypeKind::Lit(ast::Lit::Null) => Ty::Null,
            ast::TypeKind::Error => Ty::Error,
            _ => {
                self.not_implemented(t.span, "this kind of type");
                Ty::Error
            }
        }
    }

    // ------------------------------------------------------------ blocks and statements

    fn block(&mut self, b: &ast::Block) -> Block {
        self.with_scope(&b.stmts, |r| r.block_stmts(&b.stmts, b.span))
    }

    fn with_scope<T>(&mut self, stmts: &[ast::Stmt], f: impl FnOnce(&mut Resolver) -> T) -> T {
        let mut scope = Scope::default();
        for s in stmts {
            if let ast::StmtKind::Let { pat, .. } = &s.kind
                && let ast::PatternKind::Ident(n) = &pat.kind
            {
                scope.later.entry(n.name.clone()).or_insert(n.span);
            }
        }
        self.f.scopes.push(scope);
        let out = f(self);
        self.f.scopes.pop();
        out
    }

    fn block_stmts(&mut self, stmts: &[ast::Stmt], span: Span) -> Block {
        let mut out = Vec::new();
        let mut tail = None;
        for (i, s) in stmts.iter().enumerate() {
            let last = i + 1 == stmts.len();
            self.lower_stmt(s, &mut out, if last { Some(&mut tail) } else { None });
        }
        Block {
            stmts: out,
            tail,
            span,
        }
    }

    /// Lowers one statement. The last statement of a block passes `tail`, so an expression
    /// without `;` becomes the block's value.
    fn lower_stmt(
        &mut self,
        s: &ast::Stmt,
        out: &mut Vec<Stmt>,
        tail: Option<&mut Option<Box<Expr>>>,
    ) {
        match &s.kind {
            ast::StmtKind::Expr { expr, semi } => {
                let e = self.lower_expr(expr);
                match tail {
                    Some(t) if !semi => *t = Some(Box::new(e)),
                    _ => out.push(Stmt::Expr(e)),
                }
            }
            ast::StmtKind::Let {
                kind,
                pat,
                ty,
                init,
                comptime,
            } => {
                if *comptime {
                    self.not_implemented(s.span, "`comptime` bindings");
                }
                let ast::PatternKind::Ident(name) = &pat.kind else {
                    self.not_implemented(pat.span, "destructuring");
                    return;
                };
                let ty = ty.as_ref().map(|t| self.resolve_type(t));
                let init = init.as_ref().map(|e| self.lower_expr(e));
                let scope = self.f.scopes.last().expect("scope");
                if let Some(&prev) = scope.names.get(&name.name) {
                    let decl = self.f.locals[prev.0 as usize].span;
                    self.diags.push(
                        Diagnostic::error(
                            codes::DUPLICATE_DECL,
                            format!("`{}` is declared twice in the same block", name.name),
                        )
                        .primary(name.span, "declared again here")
                        .secondary(decl, "first declared here")
                        .help("use a different name, or assign to the existing `let` binding"),
                    );
                }
                let id = self.declare_local(name, *kind == ast::BindingKind::Let, ty);
                out.push(Stmt::Let {
                    var: Var::Local(id),
                    init,
                    span: s.span,
                });
            }
            ast::StmtKind::Using { .. } => self.not_implemented(s.span, "`using`"),
            ast::StmtKind::While { cond, body } => {
                let cond = self.lower_expr(cond);
                let body = self.loop_body(body, LoopKind::While);
                let e = self.expr(
                    ExprKind::While {
                        cond: Box::new(cond),
                        body: Box::new(body),
                        update: None,
                        test_first: true,
                    },
                    s.span,
                );
                out.push(Stmt::Expr(e));
            }
            ast::StmtKind::DoWhile { body, cond } => {
                self.f.loops.push(LoopKind::While);
                let b = self.block(body);
                self.f.loops.pop();
                let body = self.expr(ExprKind::Block(b), body.span);
                let cond = self.lower_expr(cond);
                let e = self.expr(
                    ExprKind::While {
                        cond: Box::new(cond),
                        body: Box::new(body),
                        update: None,
                        test_first: false,
                    },
                    s.span,
                );
                out.push(Stmt::Expr(e));
            }
            ast::StmtKind::For {
                init,
                cond,
                update,
                body,
            } => {
                let e = self.c_for(
                    init.as_deref(),
                    cond.as_ref(),
                    update.as_ref(),
                    body,
                    s.span,
                );
                out.push(Stmt::Expr(e));
            }
            ast::StmtKind::ForOf {
                is_await,
                pat,
                iter,
                body,
                ..
            } => {
                let e = self.for_of(*is_await, pat, iter, body, s.span);
                out.push(Stmt::Expr(e));
            }
            ast::StmtKind::Decl(d) => {
                let what = match &d.kind {
                    ast::DeclKind::Function(_) => "nested functions",
                    ast::DeclKind::Struct(_) => "structs",
                    ast::DeclKind::Enum(_) => "enums",
                    ast::DeclKind::Class(_) => "classes",
                    ast::DeclKind::Interface(_) => "interfaces",
                    ast::DeclKind::TypeAlias { .. } => "type aliases",
                    ast::DeclKind::Import(_)
                    | ast::DeclKind::ExportList { .. }
                    | ast::DeclKind::ExportAll { .. } => "modules",
                    ast::DeclKind::Extern { .. } => "`extern` blocks",
                    ast::DeclKind::Binding(_) => "exports",
                };
                self.not_implemented(d.span, what);
            }
            ast::StmtKind::Error => {}
        }
    }

    fn loop_body(&mut self, body: &ast::Expr, kind: LoopKind) -> Expr {
        self.f.loops.push(kind);
        let b = self.lower_expr(body);
        self.f.loops.pop();
        b
    }

    /// `for (init; cond; update) body` → `{ init; while (cond) body /* update */ }`
    fn c_for(
        &mut self,
        init: Option<&ast::Stmt>,
        cond: Option<&ast::Expr>,
        update: Option<&ast::Expr>,
        body: &ast::Expr,
        span: Span,
    ) -> Expr {
        let init_stmts: Vec<ast::Stmt> = init.into_iter().cloned().collect();
        self.with_scope(&init_stmts, |r| {
            let mut stmts = Vec::new();
            if let Some(i) = init {
                r.lower_stmt(i, &mut stmts, None);
            }
            let cond = match cond {
                Some(c) => r.lower_expr(c),
                None => r.expr(ExprKind::Bool(true), span),
            };
            let update = update.map(|u| Box::new(r.lower_expr(u)));
            let body = r.loop_body(body, LoopKind::While);
            let w = r.expr(
                ExprKind::While {
                    cond: Box::new(cond),
                    body: Box::new(body),
                    update,
                    test_first: true,
                },
                span,
            );
            stmts.push(Stmt::Expr(w));
            r.expr(
                ExprKind::Block(Block {
                    stmts,
                    tail: None,
                    span,
                }),
                span,
            )
        })
    }

    /// `for (const i of a..b) body` → a counting `while` over hidden locals.
    fn for_of(
        &mut self,
        is_await: bool,
        pat: &ast::Pattern,
        iter: &ast::Expr,
        body: &ast::Expr,
        span: Span,
    ) -> Expr {
        let ast::ExprKind::Range { start, end } = &iter.kind else {
            self.not_implemented(iter.span, "`for-of` over anything but a range");
            return self.error_expr(span);
        };
        if is_await {
            self.not_implemented(span, "`for await`");
        }
        let ast::PatternKind::Ident(name) = &pat.kind else {
            self.not_implemented(pat.span, "destructuring");
            return self.error_expr(span);
        };
        let start = self.lower_expr(start);
        let end = self.lower_expr(end);
        let i = self.hidden_local("<i>", start.span);
        let n = self.hidden_local("<end>", end.span);
        let mut stmts = vec![
            Stmt::Let {
                var: Var::Local(i),
                init: Some(start),
                span,
            },
            Stmt::Let {
                var: Var::Local(n),
                init: Some(end),
                span,
            },
        ];
        let vi = self.expr(ExprKind::Var(Var::Local(i)), span);
        let vn = self.expr(ExprKind::Var(Var::Local(n)), span);
        let cond = self.expr(
            ExprKind::Binary(ast::BinOp::Lt, Box::new(vi), Box::new(vn)),
            span,
        );
        let one = self.expr(ExprKind::Int(1, None), span);
        let update = self.expr(
            ExprKind::Assign {
                op: Some(ast::BinOp::Add),
                target: Target::Var(Var::Local(i)),
                value: Box::new(one),
            },
            span,
        );
        // The body sees a fresh binding of the loop variable each iteration.
        self.f.scopes.push(Scope::default());
        let x = self.declare_local(name, false, None);
        let vi2 = self.expr(ExprKind::Var(Var::Local(i)), name.span);
        let body = self.loop_body(body, LoopKind::While);
        self.f.scopes.pop();
        let inner = Block {
            stmts: vec![
                Stmt::Let {
                    var: Var::Local(x),
                    init: Some(vi2),
                    span: name.span,
                },
                Stmt::Expr(body),
            ],
            tail: None,
            span,
        };
        let inner = self.expr(ExprKind::Block(inner), span);
        let w = self.expr(
            ExprKind::While {
                cond: Box::new(cond),
                body: Box::new(inner),
                update: Some(Box::new(update)),
                test_first: true,
            },
            span,
        );
        stmts.push(Stmt::Expr(w));
        self.expr(
            ExprKind::Block(Block {
                stmts,
                tail: None,
                span,
            }),
            span,
        )
    }

    // ------------------------------------------------------------ expressions

    fn lower_expr(&mut self, e: &ast::Expr) -> Expr {
        use ast::ExprKind as A;
        let span = e.span;
        let kind = match &e.kind {
            A::Lit(l) => match l {
                ast::Lit::Int { value, suffix } => ExprKind::Int(*value, *suffix),
                ast::Lit::Float { value, suffix } => ExprKind::Float(*value, *suffix),
                ast::Lit::Str(s) => ExprKind::Str(s.clone()),
                ast::Lit::Bool(b) => ExprKind::Bool(*b),
                ast::Lit::Null => ExprKind::Null,
                ast::Lit::Regex { .. } => {
                    self.not_implemented(span, "regex literals");
                    ExprKind::Error
                }
            },
            A::Ident(name) => match self.lookup(name, span) {
                Some(Found::Var(v)) => ExprKind::Var(v),
                Some(Found::Fn(f)) => ExprKind::Fn(f),
                Some(Found::Builtin(b)) => ExprKind::Builtin(b),
                Some(Found::Member(field)) => {
                    let (this, _) = self.f.this.expect("member lookup implies a method");
                    let object = self.expr(ExprKind::Var(Var::Local(this)), span);
                    ExprKind::Member {
                        object: Box::new(object),
                        name: field,
                        name_span: span,
                    }
                }
                None => ExprKind::Error,
            },
            A::This => match self.f.this {
                Some((this, _)) => ExprKind::Var(Var::Local(this)),
                None => {
                    self.error(
                        codes::NOT_A_VALUE,
                        span,
                        "`this` outside a method",
                        "not inside a method",
                    );
                    ExprKind::Error
                }
            },
            A::Object { ty, fields } => self.object_literal(ty.as_deref(), fields, span),
            A::Paren(inner) => return self.lower_expr(inner),
            A::Template { tag, quasis, exprs } => {
                if tag.is_some() {
                    self.not_implemented(span, "tagged templates");
                }
                let parts = exprs
                    .iter()
                    .map(|p| TemplatePart {
                        expr: self.lower_expr(&p.expr),
                        spec: p.spec.clone(),
                    })
                    .collect();
                ExprKind::Template {
                    quasis: quasis.clone(),
                    parts,
                }
            }
            A::Unary { op, expr } => match op {
                ast::UnOp::PreInc | ast::UnOp::PreDec | ast::UnOp::PostInc | ast::UnOp::PostDec => {
                    let Some(target) = self.lower_target(expr) else {
                        return self.error_expr(span);
                    };
                    ExprKind::IncDec {
                        target,
                        inc: matches!(op, ast::UnOp::PreInc | ast::UnOp::PostInc),
                        prefix: matches!(op, ast::UnOp::PreInc | ast::UnOp::PreDec),
                    }
                }
                ast::UnOp::Typeof => {
                    self.not_implemented(span, "`typeof`");
                    ExprKind::Error
                }
                _ => ExprKind::Unary(*op, Box::new(self.lower_expr(expr))),
            },
            A::Binary { op, lhs, rhs } => ExprKind::Binary(
                *op,
                Box::new(self.lower_expr(lhs)),
                Box::new(self.lower_expr(rhs)),
            ),
            A::Assign { op, target, value } => {
                let value = self.lower_expr(value);
                let Some(target) = self.lower_target(target) else {
                    return self.error_expr(span);
                };
                ExprKind::Assign {
                    op: *op,
                    target,
                    value: Box::new(value),
                }
            }
            A::Ternary { cond, then, else_ } => ExprKind::If {
                cond: Box::new(self.lower_expr(cond)),
                then: Box::new(self.lower_expr(then)),
                else_: Some(Box::new(self.lower_expr(else_))),
            },
            A::Call {
                callee,
                type_args,
                args,
            } => {
                if !type_args.is_empty() {
                    self.not_implemented(type_args[0].span, "generic calls");
                }
                let callee = self.lower_expr(callee);
                let mut out = Vec::new();
                for a in args {
                    if a.spread || a.name.is_some() {
                        self.not_implemented(a.value.span, "named and spread arguments");
                    }
                    out.push(self.lower_expr(&a.value));
                }
                ExprKind::Call {
                    callee: Box::new(callee),
                    args: out,
                }
            }
            A::Member {
                object,
                prop,
                optional,
            } => {
                if *optional {
                    self.not_implemented(span, "`?.`");
                }
                ExprKind::Member {
                    object: Box::new(self.lower_expr(object)),
                    name: prop.name.clone(),
                    name_span: prop.span,
                }
            }
            A::Index { object, index } => ExprKind::Index {
                object: Box::new(self.lower_expr(object)),
                index: Box::new(self.lower_expr(index)),
            },
            A::As { expr, ty } => {
                let e = self.lower_expr(expr);
                let t = self.resolve_type(ty);
                ExprKind::As(Box::new(e), t)
            }
            A::Block(b) => ExprKind::Block(self.block(b)),
            A::If { cond, then, else_ } => ExprKind::If {
                cond: Box::new(self.lower_expr(cond)),
                then: Box::new(self.lower_expr(then)),
                else_: else_.as_ref().map(|e| Box::new(self.lower_expr(e))),
            },
            A::Loop(b) => {
                self.f.loops.push(LoopKind::Loop);
                let block = self.block(b);
                self.f.loops.pop();
                ExprKind::Loop(block)
            }
            A::Break(value) => {
                match self.f.loops.last() {
                    None => self.error(
                        codes::BREAK_OUTSIDE_LOOP,
                        span,
                        "`break` outside a loop",
                        "not inside `loop`, `while`, `do` or `for`",
                    ),
                    Some(LoopKind::While) if value.is_some() => self.diags.push(
                        Diagnostic::error(
                            codes::BREAK_VALUE_IN_WHILE,
                            "`break` with a value inside `while`/`for`",
                        )
                        .primary(span, "this loop can't produce a value")
                        .note("only `loop { … }` produces a value with `break value` (spec §4.3)"),
                    ),
                    _ => {}
                }
                ExprKind::Break(value.as_ref().map(|v| Box::new(self.lower_expr(v))))
            }
            A::Continue => {
                if self.f.loops.is_empty() {
                    self.error(
                        codes::BREAK_OUTSIDE_LOOP,
                        span,
                        "`continue` outside a loop",
                        "not inside a loop",
                    );
                }
                ExprKind::Continue
            }
            A::Return(value) => {
                ExprKind::Return(value.as_ref().map(|v| Box::new(self.lower_expr(v))))
            }
            A::Range { .. } => {
                self.not_implemented(span, "ranges outside `for-of`");
                ExprKind::Error
            }
            A::Error => ExprKind::Error,
            other => {
                let what = match other {
                    A::This | A::Super => "classes",
                    A::Array(_) | A::ArrayRepeat { .. } => "arrays",
                    A::Object { .. } => "object and struct literals",
                    A::New { .. } => "`new`",
                    A::NonNull(_) => "`!` (non-null assertion)",
                    A::Is { .. } | A::Match { .. } | A::Switch { .. } => "pattern matching",
                    A::Instanceof { .. } => "`instanceof`",
                    A::Function(_) => "closures",
                    A::Try { .. } | A::TryOpt(_) | A::TryBang(_) | A::Throw(_) => "exceptions",
                    A::Await(_) => "`await`",
                    A::Yield { .. } => "generators",
                    A::Tailcall(_) => "`tailcall`",
                    A::Comptime(_) => "`comptime`",
                    A::Unsafe(_) => "`unsafe`",
                    A::Arena(_) => "arenas",
                    _ => "this expression",
                };
                self.not_implemented(span, what);
                ExprKind::Error
            }
        };
        self.expr(kind, span)
    }
}

impl Resolver {
    /// An assignment target: a variable, an implicit member, or `object.field`.
    fn lower_target(&mut self, e: &ast::Expr) -> Option<Target> {
        let t = strip_parens(e);
        match &t.kind {
            ast::ExprKind::Ident(name) => self.resolve_target(name, t.span),
            ast::ExprKind::Member {
                object,
                prop,
                optional: false,
            } => {
                let object = self.lower_expr(object);
                let id = self.expr(ExprKind::Error, t.span).id;
                Some(Target::Field {
                    id,
                    object: Box::new(object),
                    name: prop.name.clone(),
                    name_span: prop.span,
                })
            }
            _ => {
                self.not_implemented(e.span, "assignment to anything but a variable or a field");
                None
            }
        }
    }

    /// `Point { x: 1, y }` or `{ x: 1, y }`.
    fn object_literal(
        &mut self,
        ty: Option<&ast::Type>,
        fields: &[ast::ObjField],
        span: Span,
    ) -> ExprKind {
        let sid = match ty.map(|t| &t.kind) {
            None => None,
            Some(ast::TypeKind::Path { path, args }) if path.len() == 1 => {
                if !args.is_empty() {
                    self.not_implemented(ty.unwrap().span, "generic structs");
                }
                match self.top_structs.get(&path[0].name) {
                    Some(id) => Some(*id),
                    None => {
                        let what = if Ty::from_name(&path[0].name).is_some() {
                            "isn't a struct"
                        } else {
                            "isn't declared"
                        };
                        self.error(
                            codes::UNKNOWN_TYPE,
                            path[0].span,
                            format!("cannot find struct `{}`", path[0].name),
                            format!("`{}` {what}", path[0].name),
                        );
                        return ExprKind::Error;
                    }
                }
            }
            Some(_) => {
                self.not_implemented(span, "this kind of struct literal");
                return ExprKind::Error;
            }
        };
        let mut inits = Vec::new();
        for f in fields {
            match f {
                ast::ObjField::KeyValue {
                    key: ast::PropKey::Ident(k),
                    value,
                } => {
                    let value = self.lower_expr(value);
                    inits.push(FieldInit {
                        name: k.name.clone(),
                        name_span: k.span,
                        value,
                    });
                }
                ast::ObjField::Shorthand(k) => {
                    let ident = ast::Expr {
                        kind: ast::ExprKind::Ident(k.name.clone()),
                        span: k.span,
                    };
                    let value = self.lower_expr(&ident);
                    inits.push(FieldInit {
                        name: k.name.clone(),
                        name_span: k.span,
                        value,
                    });
                }
                ast::ObjField::KeyValue {
                    key: ast::PropKey::Str(_, s),
                    ..
                } => self.not_implemented(*s, "string keys in object literals"),
                ast::ObjField::Spread(e) => {
                    self.not_implemented(e.span, "spread in object literals")
                }
            }
        }
        ExprKind::Object {
            ty: sid,
            fields: inits,
        }
    }
}

fn strip_parens(e: &ast::Expr) -> &ast::Expr {
    match &e.kind {
        ast::ExprKind::Paren(inner) => strip_parens(inner),
        _ => e,
    }
}

fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut cur = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            let sub = prev[j] + usize::from(ca != cb);
            cur.push(sub.min(prev[j + 1] + 1).min(cur[j] + 1));
        }
        prev = cur;
    }
    prev[b.len()]
}
