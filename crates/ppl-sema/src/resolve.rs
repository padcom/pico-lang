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
use crate::ty::{LitVal, Ty};
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
        enums: Vec::new(),
        top_enums: HashMap::new(),
        variant_names: HashSet::new(),
        stmt_switch: false,
        cond_scope: false,
        aliases: HashMap::new(),
        alias_cache: HashMap::new(),
        resolving_aliases: Vec::new(),
        unions: Vec::new(),
        cur_fn: FnId(0),
        distincts: 0,
        outer_fns: Vec::new(),
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
    /// Inside a class's code (including static methods and static initializers): the class,
    /// whose static members are in scope.
    owner: Option<StructId>,
    /// A generic function's type parameters, by name.
    type_params: Vec<(String, Ty)>,
    /// A closure: may see the enclosing functions' locals (`captures` records them).
    is_closure: bool,
    /// An arrow function (sees the enclosing method's `this`).
    arrow: bool,
    captures: Vec<Capture>,
}

struct Resolver {
    file: FileId,
    diags: Vec<Diagnostic>,
    fns: Vec<Option<Function>>,
    globals: Vec<Binding>,
    spans: Vec<Span>,
    /// Top-level functions by name (several for overloads).
    top_fns: HashMap<String, Vec<FnId>>,
    top_globals: HashMap<String, GlobalId>,
    globals_declared: HashSet<GlobalId>,
    f: FnState,
    structs: Vec<StructDef>,
    top_structs: HashMap<String, StructId>,
    enums: Vec<EnumDef>,
    top_enums: HashMap<String, EnumId>,
    /// Every variant name of every enum: an identifier pattern with one of these names is a
    /// variant pattern, not a binding.
    variant_names: HashSet<String>,
    /// The `switch` being lowered is a statement (its value isn't used).
    stmt_switch: bool,
    /// Lowering an `if`/`while` condition, where `is` bindings stay in scope for the body.
    cond_scope: bool,
    /// `type Name = T` declarations, resolved on first use.
    aliases: HashMap<String, (ast::Type, Span)>,
    alias_cache: HashMap<String, Ty>,
    resolving_aliases: Vec<String>,
    /// Interned unions: the members and the anonymous enum that stores them.
    unions: Vec<(Vec<Ty>, EnumId)>,
    /// The function being resolved (owner of its type parameters).
    cur_fn: FnId,
    /// Distinct types created so far (each gets its own id).
    distincts: u32,
    /// The states of the functions enclosing the closure being resolved, outermost first.
    outer_fns: Vec<FnState>,
}

enum Found {
    Var(Var),
    Fn(FnId),
    Overloads(Vec<FnId>),
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
        let mut enum_decls: Vec<(EnumId, &ast::EnumDecl)> = Vec::new();
        let mut class_decls: Vec<(StructId, &ast::ClassDecl)> = Vec::new();
        let mut iface_decls: Vec<(StructId, &ast::InterfaceDecl)> = Vec::new();
        let mut pending_globals: Vec<(GlobalId, &ast::Type)> = Vec::new();
        let mut first_seen: HashMap<String, Span> = HashMap::new();
        // Type aliases first: any type annotation may use them.
        for stmt in &module.stmts {
            if let ast::StmtKind::Decl(d) = &stmt.kind
                && let ast::DeclKind::TypeAlias { name, generics, ty } = &d.kind
            {
                if let Some(g) = generics.first() {
                    self.not_implemented(g.span, "generic type aliases");
                }
                if self.check_duplicate(&mut first_seen, name) {
                    continue;
                }
                self.aliases
                    .insert(name.name.clone(), (ty.clone(), name.span));
            }
        }
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
                        self.structs.push(StructDef::new(
                            sd.name.name.clone(),
                            sd.name.span,
                            false,
                        ));
                        self.top_structs.insert(sd.name.name.clone(), id);
                        struct_decls.push((id, sd));
                    }
                    if let ast::DeclKind::Interface(id_) = &d.kind {
                        if let Some(dec) = d.decorators.first() {
                            self.not_implemented(dec.span, "decorators");
                        }
                        if self.check_duplicate(&mut first_seen, &id_.name) {
                            continue;
                        }
                        let id = StructId(self.structs.len() as u32);
                        let mut def = StructDef::new(id_.name.name.clone(), id_.name.span, false);
                        def.is_interface = true;
                        self.structs.push(def);
                        self.top_structs.insert(id_.name.name.clone(), id);
                        iface_decls.push((id, id_));
                    }
                    if let ast::DeclKind::Class(cd) = &d.kind {
                        let derives = self.derive_decorators(&d.decorators);
                        if self.check_duplicate(&mut first_seen, &cd.name) {
                            continue;
                        }
                        let id = StructId(self.structs.len() as u32);
                        let mut def = StructDef::new(cd.name.name.clone(), cd.name.span, true);
                        def.derives = derives;
                        self.structs.push(def);
                        self.top_structs.insert(cd.name.name.clone(), id);
                        class_decls.push((id, cd));
                    }
                    if let ast::DeclKind::Enum(ed) = &d.kind {
                        if let Some(dec) = d.decorators.first() {
                            self.not_implemented(dec.span, "decorators");
                        }
                        if self.check_duplicate(&mut first_seen, &ed.name) {
                            continue;
                        }
                        let id = EnumId(self.enums.len() as u32);
                        self.enums.push(EnumDef {
                            name: ed.name.name.clone(),
                            name_span: ed.name.span,
                            variants: Vec::new(),
                            origin: EnumOrigin::Declared,
                        });
                        self.top_enums.insert(ed.name.name.clone(), id);
                        for v in &ed.variants {
                            self.variant_names.insert(v.name.name.clone());
                        }
                        enum_decls.push((id, ed));
                    }
                    if let ast::DeclKind::Function(f) = &d.kind
                        && let Some(name) = &f.name
                    {
                        if !d.decorators.is_empty() {
                            self.not_implemented(d.decorators[0].span, "decorators");
                        }
                        // Functions may share a name (overloads); anything else may not.
                        let overload = self.top_fns.contains_key(&name.name);
                        if !overload && self.check_duplicate(&mut first_seen, name) {
                            continue;
                        }
                        let id = FnId(self.fns.len() as u32);
                        self.fns.push(None);
                        self.top_fns.entry(name.name.clone()).or_default().push(id);
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
                    // A destructuring pattern's annotation types the whole value, so its
                    // names get their types from the initializer.
                    let ann = match &pat.kind {
                        ast::PatternKind::Ident(_) => ty.as_ref(),
                        _ => None,
                    };
                    let mut names = Vec::new();
                    pattern_idents(pat, &mut names);
                    for name in names {
                        if self.check_duplicate(&mut first_seen, name) {
                            continue;
                        }
                        let id = GlobalId(self.globals.len() as u32);
                        // Annotations are resolved once every type is declared.
                        if let Some(t) = ann {
                            pending_globals.push((id, t));
                        }
                        self.globals.push(Binding {
                            name: name.name.clone(),
                            mutable: *kind == ast::BindingKind::Let,
                            ty: None,
                            span: name.span,
                            boxed: false,
                        });
                        self.top_globals.insert(name.name.clone(), id);
                    }
                }
                _ => {}
            }
        }

        // Generic types' parameters, so annotations like `Stack<i64>` can be checked.
        let generic_decls: Vec<(StructId, &[ast::GenericParam])> = struct_decls
            .iter()
            .map(|(s, d)| (*s, &d.generics[..]))
            .chain(class_decls.iter().map(|(s, d)| (*s, &d.generics[..])))
            .collect();
        for (sid, gs) in generic_decls {
            let tps = self.type_params(gs, &[], Ty::type_owner(sid));
            self.f.type_params.clear();
            self.structs[sid.0 as usize].generics = tps;
        }
        for (g, t) in pending_globals {
            let ty = self.resolve_type(t);
            self.globals[g.0 as usize].ty = Some(ty);
        }

        // Pass 1b: interfaces first (implementers need their method lists), then struct
        // fields and methods (all type names are known now).
        let mut iface_methods = Vec::new();
        for (sid, id_) in self.order_interfaces(&iface_decls) {
            self.interface_members(sid, id_, &mut iface_methods);
        }
        for (sid, sd) in &struct_decls {
            self.struct_members(*sid, sd, &mut fn_decls);
            self.f.type_params.clear();
        }
        for (eid, ed) in &enum_decls {
            self.enum_variants(*eid, ed);
        }
        let mut classes = Vec::new();
        for (sid, cd) in self.order_classes(&class_decls) {
            classes.push(self.class_members(sid, cd));
            self.f.type_params.clear();
        }
        self.check_struct_cycles();
        self.check_conformance();

        // Pass 2: function and method bodies.
        for (id, f, sid, is_abstract) in iface_methods {
            let func = if is_abstract {
                self.abstract_stub(f, sid)
            } else {
                self.function_in(f, false, Some(sid), Some(sid))
            };
            self.fns[id.0 as usize] = Some(func);
        }
        for (id, f, this) in fn_decls {
            self.cur_fn = id;
            let func = self.function(f, false, this);
            self.fns[id.0 as usize] = Some(func);
        }
        // Overloads must differ in their parameter types.
        let sets: Vec<Vec<FnId>> = self
            .top_fns
            .values()
            .filter(|v| v.len() > 1)
            .cloned()
            .collect();
        for set in sets {
            for (i, a) in set.iter().enumerate() {
                for b in &set[..i] {
                    let tys = |r: &Resolver, f: FnId| -> Vec<Option<Ty>> {
                        let func = r.fns[f.0 as usize].as_ref().expect("resolved");
                        func.params
                            .iter()
                            .map(|p| func.local(*p).ty.clone())
                            .collect()
                    };
                    if tys(self, *a) == tys(self, *b) {
                        let (fa, fb) = (
                            self.fns[a.0 as usize].as_ref().expect("resolved"),
                            self.fns[b.0 as usize].as_ref().expect("resolved"),
                        );
                        let d = Diagnostic::error(
                            codes::DUPLICATE_DECL,
                            format!("`{}` is declared twice", fa.name),
                        )
                        .primary(fa.name_span, "declared again here")
                        .secondary(fb.name_span, "first declared here")
                        .note("overloads must differ in their parameter types");
                        self.diags.push(d);
                    }
                }
            }
        }
        let mut static_inits = Vec::new();
        for c in &classes {
            for (id, f, this, is_abstract) in &c.methods {
                self.cur_fn = *id;
                let func = if *is_abstract {
                    if f.body.is_some() {
                        self.error(
                            codes::ABSTRACT_NOT_IMPLEMENTED,
                            f.span,
                            "an abstract method has no body",
                            "remove the body, or the `abstract`",
                        );
                    }
                    self.abstract_stub(f, c.sid)
                } else {
                    self.function_in(f, false, *this, Some(c.sid))
                };
                self.fns[id.0 as usize] = Some(func);
            }
            for (id, decl) in &c.ctors {
                let ctor = self.constructor(c, *id, *decl);
                self.fns[id.0 as usize] = Some(ctor);
            }
            if let Some(id) = c.static_init {
                let init = self.static_init(c);
                self.fns[id.0 as usize] = Some(init);
                static_inits.push(id);
            }
        }

        // Pass 3: the entry body (top-level statements in order).
        let entry_id = FnId(self.fns.len() as u32);
        self.fns.push(None);
        self.cur_fn = entry_id;
        self.f = FnState {
            in_entry: true,
            ..Default::default()
        };
        self.f.scopes.push(Scope::default());
        let mut block = self.top_level(&module.stmts, module.span);
        // Classes' static fields are initialized before any top-level code runs.
        let mut first = Vec::new();
        for id in &static_inits {
            let callee = self.expr(ExprKind::Fn(*id), module.span.shrink_to_lo());
            let call = self.expr(
                ExprKind::Call {
                    callee: Box::new(callee),
                    args: Vec::new(),
                    type_args: Vec::new(),
                },
                module.span.shrink_to_lo(),
            );
            first.push(Stmt::Expr(call));
        }
        first.append(&mut block.stmts);
        block.stmts = first;
        let locals = std::mem::take(&mut self.f.locals);
        self.fns[entry_id.0 as usize] = Some(Function {
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
            owner: None,
            generics: Vec::new(),
            closure: None,
            defaults: Vec::new(),
            rest: false,
            throws: Vec::new(),
            infer_throws: false,
        });

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
            enums: std::mem::take(&mut self.enums),
            error_class: self.top_structs.get("Error").copied(),
            static_inits,
        }
    }

    fn enum_variants(&mut self, eid: EnumId, ed: &ast::EnumDecl) {
        if let Some(g) = ed.generics.first() {
            self.not_implemented(g.span, "generic enums");
        }
        if let Some(t) = ed.implements.first() {
            self.not_implemented(t.span, "interfaces");
        }
        if let Some(m) = ed.members.first() {
            self.not_implemented(m.span, "enum methods");
        }
        let mut seen: HashMap<String, Span> = HashMap::new();
        for v in &ed.variants {
            if let Some(dec) = v.decorators.first() {
                self.not_implemented(dec.span, "decorators");
            }
            if self.check_duplicate(&mut seen, &v.name) {
                continue;
            }
            let mut fields = Vec::new();
            let mut field_seen: HashMap<String, Span> = HashMap::new();
            for p in v.fields.iter().flatten() {
                let ast::PatternKind::Ident(fname) = &p.pat.kind else {
                    self.not_implemented(p.pat.span, "destructured variant fields");
                    continue;
                };
                if p.rest || p.optional || p.default.is_some() || p.comptime {
                    self.not_implemented(p.span, "optional, default and rest variant fields");
                }
                if self.check_duplicate(&mut field_seen, fname) {
                    continue;
                }
                let ty = match &p.ty {
                    Some(t) => self.resolve_type(t),
                    None => {
                        self.error(
                            codes::MISSING_PARAM_TYPE,
                            fname.span,
                            format!("variant field `{}` needs a type", fname.name),
                            "add a type annotation, e.g. `r: f64`",
                        );
                        Ty::Error
                    }
                };
                fields.push(FieldDef {
                    name: fname.name.clone(),
                    ty,
                    span: fname.span,
                    preassigned: false,
                    readonly: false,
                });
            }
            self.enums[eid.0 as usize].variants.push(VariantDef {
                name: v.name.name.clone(),
                span: v.name.span,
                fields,
                has_parens: v.fields.is_some(),
            });
        }
    }

    fn enum_ty(&self, id: EnumId) -> Ty {
        Ty::Enum(id, self.enums[id.0 as usize].name.as_str().into())
    }

    fn struct_members<'a>(
        &mut self,
        sid: StructId,
        sd: &'a ast::StructDecl,
        fn_decls: &mut Vec<(FnId, &'a ast::FnDecl, Option<StructId>)>,
    ) {
        let generics = self.type_params(&sd.generics, &[], Ty::type_owner(sid));
        self.structs[sid.0 as usize].generics = generics;
        if let (Some(g), Some(t)) = (sd.generics.first(), sd.implements.first()) {
            let _ = g;
            self.not_implemented(t.span, "generic types implementing interfaces");
        }
        self.resolve_implements(sid, &sd.implements);
        let mut seen: HashMap<String, Span> = HashMap::new();
        for m in &sd.members {
            if let Some(dec) = m.decorators.first() {
                self.not_implemented(dec.span, "decorators");
            }
            let mods_ok = ast::Modifiers {
                readonly: false,
                ..m.mods
            };
            if !mods_ok.is_empty() {
                self.not_implemented(m.span, "member modifiers other than `readonly`");
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
                    let readonly = m.mods.readonly;
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
                        preassigned: false,
                        readonly,
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
                    if m.mods.readonly {
                        self.structs[sid.0 as usize]
                            .readonly_methods
                            .insert(name.name.clone());
                    }
                    fn_decls.push((id, f, Some(sid)));
                }
                ast::MemberKind::Operator { op, func } => {
                    let (name, decl) = operator_method(op, func, m.span);
                    let id = FnId(self.fns.len() as u32);
                    self.fns.push(None);
                    self.structs[sid.0 as usize].methods.push((name, id));
                    fn_decls.push((id, decl, Some(sid)));
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

    /// A struct or enum that contains itself by value (directly or through other inline
    /// types) has no size.
    fn check_struct_cycles(&mut self) {
        fn parts(r: &Resolver, n: Inline) -> Vec<Inline> {
            let mut out = Vec::new();
            match n {
                Inline::Struct(id) => {
                    for f in &r.structs[id.0 as usize].fields {
                        inline_types(&f.ty, &mut out);
                    }
                }
                Inline::Enum(id) => {
                    for v in &r.enums[id.0 as usize].variants {
                        for f in &v.fields {
                            inline_types(&f.ty, &mut out);
                        }
                    }
                }
            }
            out
        }
        fn visit(r: &Resolver, n: Inline, path: &mut Vec<Inline>) -> Option<Vec<Inline>> {
            if let Some(pos) = path.iter().position(|p| *p == n) {
                return Some(path[pos..].to_vec());
            }
            path.push(n);
            for inner in parts(r, n) {
                if let Some(c) = visit(r, inner, path) {
                    return Some(c);
                }
            }
            path.pop();
            None
        }
        let name = |r: &Resolver, n: Inline| match n {
            Inline::Struct(id) => r.structs[id.0 as usize].name.clone(),
            Inline::Enum(id) => r.enums[id.0 as usize].name.clone(),
        };
        let nodes: Vec<Inline> = (0..self.structs.len())
            .map(|i| Inline::Struct(StructId(i as u32)))
            .chain((0..self.enums.len()).map(|i| Inline::Enum(EnumId(i as u32))))
            .collect();
        let mut reported: HashSet<Inline> = HashSet::new();
        for n in nodes {
            if reported.contains(&n) {
                continue;
            }
            if let Some(cycle) = visit(self, n, &mut Vec::new())
                && cycle[0] == n
            {
                reported.extend(cycle.iter().copied());
                let names: Vec<String> = cycle.iter().map(|c| name(self, *c)).collect();
                let (kind, span) = match n {
                    Inline::Struct(id) => ("struct", self.structs[id.0 as usize].name_span),
                    Inline::Enum(id) => ("enum", self.enums[id.0 as usize].name_span),
                };
                let d = Diagnostic::error(
                    codes::RECURSIVE_STRUCT,
                    format!("{kind} `{}` contains itself", names[0]),
                )
                .primary(span, format!("this {kind} would have infinite size"))
                .note(format!("{} → {}", names.join(" → "), names[0]))
                .help("structs and enums are stored inline; use a class or an array for recursive data");
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
                    kind,
                    pat,
                    ty,
                    init,
                    comptime: false,
                } => {
                    let ast::PatternKind::Ident(name) = &pat.kind else {
                        let ty = ty.as_ref().map(|t| self.resolve_type(t));
                        match init {
                            Some(e) => {
                                let init = self.lower_expr(e);
                                let mutable = *kind == ast::BindingKind::Let;
                                self.destructure(pat, init, mutable, ty, &mut out, true);
                            }
                            None => self.destructure_without_init(pat),
                        }
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
                        ast::DeclKind::Function(_)
                            | ast::DeclKind::Struct(_)
                            | ast::DeclKind::Enum(_)
                            | ast::DeclKind::Class(_)
                            | ast::DeclKind::Interface(_)
                            | ast::DeclKind::TypeAlias { .. }
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
        let owner = this_struct;
        self.function_in(f, is_entry, this_struct, owner)
    }

    fn function_in(
        &mut self,
        f: &ast::FnDecl,
        is_entry: bool,
        this_struct: Option<StructId>,
        owner: Option<StructId>,
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
        self.f = FnState {
            in_entry: is_entry,
            owner,
            ..Default::default()
        };
        self.f.scopes.push(Scope::default());
        if let Some(sid) = owner {
            self.enter_type_params(sid);
        }
        let generics = self.type_params(&f.generics, &f.where_clause, self.cur_fn.0);
        // `throws E` may name the function's type parameters.
        let throws = match &f.throws {
            Some(t) => self.throws_clause(t),
            None => Vec::new(),
        };
        // Top-level functions without a `throws` clause infer it (until modules make some of
        // them exported, which must declare it).
        let infer_throws = f.throws.is_none() && owner.is_none() && !is_entry;
        let mut defaults: Vec<Option<Expr>> = Vec::new();
        let mut rest = false;
        if let Some(sid) = this_struct {
            let ty = self.struct_ty(sid);
            let id = LocalId(self.f.locals.len() as u32);
            self.f.locals.push(Binding {
                name: "this".into(),
                mutable: false,
                ty: Some(ty),
                span: name.span,
                boxed: false,
            });
            self.f.this = Some((id, sid));
        }
        let mut params = Vec::new();
        for p in &f.params {
            let ast::PatternKind::Ident(pname) = &p.pat.kind else {
                self.not_implemented(p.pat.span, "destructured parameters");
                continue;
            };
            if p.comptime || !p.mods.is_empty() {
                self.not_implemented(p.span, "comptime parameters and parameter modifiers");
            }
            // A default sees the parameters before it.
            let default = match (&p.default, p.optional) {
                (Some(d), _) => Some(self.lower_expr(d)),
                (None, true) => Some(self.expr(ExprKind::Null, p.span)),
                (None, false) => None,
            };
            if p.rest {
                rest = true;
            }
            let ty = match &p.ty {
                Some(t) => {
                    let t = self.resolve_type(t);
                    Some(if p.optional { t.nullable() } else { t })
                }
                None if p.default.is_some() => None,
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
            defaults.push(default);
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
        if rest && !f.params.last().is_some_and(|p| p.rest) {
            self.error(
                codes::INVALID_ASSIGN_TARGET,
                f.span,
                "a rest parameter must be the last one",
                "move `...` to the last parameter",
            );
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
            owner,
            generics,
            closure: None,
            defaults,
            rest,
            throws,
            infer_throws,
        }
    }

    /// `throws A | B`: the error classes (each must extend `Error`).
    fn throws_clause(&mut self, t: &ast::Type) -> Vec<Ty> {
        let members: Vec<&ast::Type> = match &t.kind {
            ast::TypeKind::Union(ms) => ms.iter().collect(),
            _ => vec![t],
        };
        let mut out = Vec::new();
        for m in members {
            match self.resolve_type(m) {
                ty @ (Ty::Class(..) | Ty::Param(..)) => out.push(ty),
                Ty::Never => {}
                Ty::Error => {}
                other => self.error(
                    codes::NOT_THROWABLE,
                    m.span,
                    format!("`{other}` can't be thrown"),
                    "`throws` lists `Error` classes",
                ),
            }
        }
        out
    }

    /// Declares a generic function's type parameters (as `Ty::Param` of the current
    /// function) and resolves their bounds, from `extends` and `where`.
    /// Makes a generic type's parameters visible (inside its members' code).
    fn enter_type_params(&mut self, sid: StructId) {
        let owner = Ty::type_owner(sid);
        let names: Vec<String> = self.structs[sid.0 as usize]
            .generics
            .iter()
            .map(|p| p.name.clone())
            .collect();
        for (i, n) in names.into_iter().enumerate() {
            let t = Ty::Param(owner, i as u32, n.as_str().into());
            self.f.type_params.push((n, t));
        }
    }

    fn type_params(
        &mut self,
        params: &[ast::GenericParam],
        wheres: &[ast::WherePredicate],
        owner: u32,
    ) -> Vec<TypeParam> {
        let mut out: Vec<TypeParam> = Vec::new();
        for (i, g) in params.iter().enumerate() {
            let ast::GenericKind::Type { default, .. } = &g.kind else {
                self.not_implemented(g.span, "`comptime` parameters");
                continue;
            };
            let _ = default;
            self.f.type_params.push((
                g.name.name.clone(),
                Ty::Param(owner, i as u32, g.name.name.as_str().into()),
            ));
            out.push(TypeParam {
                name: g.name.name.clone(),
                span: g.name.span,
                bounds: Vec::new(),
                default: None,
            });
        }
        for (i, g) in params.iter().enumerate() {
            if let ast::GenericKind::Type {
                default: Some(d), ..
            } = &g.kind
            {
                let t = self.resolve_type(d);
                if let Some(tp) = out.get_mut(i) {
                    tp.default = Some(t);
                }
            }
        }
        // Bounds may mention any of the parameters, so they're resolved afterwards.
        for (i, g) in params.iter().enumerate() {
            if let ast::GenericKind::Type { bound: Some(b), .. } = &g.kind {
                let bs = self.bounds(b);
                if let Some(tp) = out.get_mut(i) {
                    tp.bounds.extend(bs);
                }
            }
        }
        for w in wheres {
            let target = match &w.ty.kind {
                ast::TypeKind::Path { path, args } if path.len() == 1 && args.is_empty() => {
                    out.iter().position(|p| p.name == path[0].name)
                }
                _ => None,
            };
            match target {
                Some(i) => {
                    let bs = self.bounds(&w.bound);
                    out[i].bounds.extend(bs);
                }
                None => self.error(
                    codes::UNKNOWN_TYPE,
                    w.ty.span,
                    "a `where` clause constrains a type parameter of this function",
                    "not a type parameter",
                ),
            }
        }
        out
    }

    /// `Comparable<T>`, `Equatable<T>`, an interface, or several joined with `&`.
    fn bounds(&mut self, t: &ast::Type) -> Vec<Bound> {
        match &t.kind {
            ast::TypeKind::Intersection(parts) => {
                parts.iter().flat_map(|p| self.bounds(p)).collect()
            }
            ast::TypeKind::Path { path, .. } if path.len() == 1 && path[0].name == "Comparable" => {
                vec![Bound::Comparable]
            }
            ast::TypeKind::Path { path, .. } if path.len() == 1 && path[0].name == "Equatable" => {
                vec![Bound::Equatable]
            }
            _ => match self.resolve_type(t) {
                Ty::Iface(sid, _, _) => vec![Bound::Iface(sid)],
                // `E extends Error`: what a function throws.
                Ty::Class(sid, _, _) if Some(sid) == self.top_structs.get("Error").copied() => {
                    vec![Bound::Error]
                }
                Ty::Error => Vec::new(),
                other => {
                    self.error(
                        codes::UNKNOWN_TYPE,
                        t.span,
                        format!("`{other}` can't be a bound"),
                        "bounds are interfaces",
                    );
                    Vec::new()
                }
            },
        }
    }

    /// Classes ordered so every base comes before its subclasses; reports unknown bases,
    /// `final` bases and inheritance cycles.
    fn order_classes<'a>(
        &mut self,
        decls: &[(StructId, &'a ast::ClassDecl)],
    ) -> Vec<(StructId, &'a ast::ClassDecl)> {
        let mut base_of: HashMap<StructId, StructId> = HashMap::new();
        for (sid, cd) in decls {
            let Some(t) = &cd.extends else { continue };
            let base = match &t.kind {
                ast::TypeKind::Path { path, args } if path.len() == 1 && args.is_empty() => self
                    .top_structs
                    .get(&path[0].name)
                    .copied()
                    .filter(|b| self.structs[b.0 as usize].is_class),
                _ => None,
            };
            match base {
                Some(b) => {
                    base_of.insert(*sid, b);
                }
                None => self.error(
                    codes::UNKNOWN_TYPE,
                    t.span,
                    "a class can only extend another class",
                    "not a class",
                ),
            }
        }
        let mut out = Vec::new();
        let mut done: HashSet<StructId> = HashSet::new();
        for (sid, _) in decls {
            let mut chain = vec![*sid];
            let mut cur = *sid;
            while let Some(b) = base_of.get(&cur) {
                if chain.contains(b) {
                    let name = self.structs[sid.0 as usize].name.clone();
                    let span = self.structs[sid.0 as usize].name_span;
                    self.error(
                        codes::RECURSIVE_STRUCT,
                        span,
                        format!("class `{name}` inherits from itself"),
                        "inheritance cycle",
                    );
                    base_of.remove(&cur);
                    break;
                }
                chain.push(*b);
                cur = *b;
            }
            for c in chain.into_iter().rev() {
                if done.insert(c) {
                    let decl = decls.iter().find(|(s, _)| *s == c).expect("declared").1;
                    out.push((c, decl));
                }
            }
        }
        for (sid, b) in base_of {
            self.structs[sid.0 as usize].base = Some(b);
        }
        out
    }

    /// The interfaces in `types` (for `implements` / `extends`).
    fn resolve_implements(&mut self, sid: StructId, types: &[ast::Type]) {
        for t in types {
            match self.resolve_type(t) {
                Ty::Iface(i, _, _) => self.structs[sid.0 as usize].implements.push(i),
                Ty::Error => {}
                other => self.error(
                    codes::UNKNOWN_TYPE,
                    t.span,
                    format!("`{other}` isn't an interface"),
                    "only interfaces can be implemented",
                ),
            }
        }
    }

    /// Interfaces ordered so every interface comes after the ones it extends.
    fn order_interfaces<'a>(
        &mut self,
        decls: &[(StructId, &'a ast::InterfaceDecl)],
    ) -> Vec<(StructId, &'a ast::InterfaceDecl)> {
        for (sid, d) in decls {
            self.resolve_implements(*sid, &d.extends);
        }
        let mut out = Vec::new();
        let mut state: HashMap<StructId, u8> = HashMap::new();
        fn visit<'a>(
            r: &mut Resolver,
            sid: StructId,
            decls: &[(StructId, &'a ast::InterfaceDecl)],
            state: &mut HashMap<StructId, u8>,
            out: &mut Vec<(StructId, &'a ast::InterfaceDecl)>,
        ) {
            match state.get(&sid) {
                Some(2) => return,
                Some(1) => {
                    let (name, span) = {
                        let d = &r.structs[sid.0 as usize];
                        (d.name.clone(), d.name_span)
                    };
                    r.error(
                        codes::RECURSIVE_STRUCT,
                        span,
                        format!("interface `{name}` extends itself"),
                        "inheritance cycle",
                    );
                    r.structs[sid.0 as usize].implements.clear();
                    return;
                }
                _ => {}
            }
            state.insert(sid, 1);
            let parents = r.structs[sid.0 as usize].implements.clone();
            for p in parents {
                visit(r, p, decls, state, out);
            }
            state.insert(sid, 2);
            if let Some((_, d)) = decls.iter().find(|(s, _)| *s == sid) {
                out.push((sid, *d));
            }
        }
        for (sid, _) in decls {
            visit(self, *sid, decls, &mut state, &mut out);
        }
        out
    }

    /// An interface's methods: the inherited ones first (so the first parent's method table
    /// is a prefix), then its own; a body is a default implementation.
    fn interface_members<'a>(
        &mut self,
        sid: StructId,
        d: &'a ast::InterfaceDecl,
        out: &mut Vec<(FnId, &'a ast::FnDecl, StructId, bool)>,
    ) {
        if let Some(g) = d.generics.first() {
            self.not_implemented(g.span, "generic interfaces");
        }
        let mut methods: Vec<(String, FnId)> = Vec::new();
        let mut abstract_fns = HashSet::new();
        for p in self.structs[sid.0 as usize].implements.clone() {
            let pd = &self.structs[p.0 as usize];
            for (n, f) in pd.methods.clone() {
                if !methods.iter().any(|(m, _)| *m == n) {
                    if pd.abstract_fns.contains(&f) {
                        abstract_fns.insert(f);
                    }
                    methods.push((n, f));
                }
            }
        }
        let mut seen: HashMap<String, Span> = HashMap::new();
        for m in &d.members {
            let ast::MemberKind::Method(f) = &m.kind else {
                self.not_implemented(m.span, "interface members other than methods");
                continue;
            };
            let Some(name) = &f.name else { continue };
            if self.check_duplicate(&mut seen, name) {
                continue;
            }
            let id = FnId(self.fns.len() as u32);
            self.fns.push(None);
            let is_abstract = f.body.is_none();
            match methods.iter_mut().find(|(n, _)| *n == name.name) {
                // Redeclaring an inherited method (to give it a default) keeps its slot.
                Some(slot) => {
                    abstract_fns.remove(&slot.1);
                    slot.1 = id;
                }
                None => methods.push((name.name.clone(), id)),
            }
            if is_abstract {
                abstract_fns.insert(id);
            }
            out.push((id, f, sid, is_abstract));
        }
        let def = &mut self.structs[sid.0 as usize];
        def.methods = methods;
        def.abstract_fns = abstract_fns;
    }

    /// Every interface a type implements, directly or through base classes and interface
    /// inheritance.
    fn all_interfaces(&self, sid: StructId, out: &mut Vec<StructId>) {
        let d = &self.structs[sid.0 as usize];
        for i in &d.implements {
            if !out.contains(i) {
                out.push(*i);
                self.all_interfaces(*i, out);
            }
        }
        if let Some(b) = d.base {
            self.all_interfaces(b, out);
        }
    }

    /// Each struct and class must provide every method of its interfaces (or inherit a default
    /// for it, unless two interfaces' defaults conflict). Interface methods are public.
    fn check_conformance(&mut self) {
        for i in 0..self.structs.len() {
            let sid = StructId(i as u32);
            let d = &self.structs[i];
            if d.is_interface || d.implements.is_empty() && d.base.is_none() {
                continue;
            }
            let mut ifaces = Vec::new();
            self.all_interfaces(sid, &mut ifaces);
            let mut errors = Vec::new();
            let mut checked: HashSet<String> = HashSet::new();
            for iface in &ifaces {
                let idef = &self.structs[iface.0 as usize];
                for (name, f) in &idef.methods {
                    if !checked.insert(name.clone()) {
                        continue;
                    }
                    let d = &self.structs[i];
                    if d.method(name).is_some() {
                        if d.is_class && !d.is_public(name) {
                            errors.push((
                                codes::PRIVATE_MEMBER,
                                format!(
                                    "`{}.{name}` implements `{}.{name}`, so it must be `public`",
                                    d.name, idef.name
                                ),
                            ));
                        }
                        continue;
                    }
                    // Defaults for this method from all the type's interfaces.
                    let mut defaults: Vec<(String, FnId)> = Vec::new();
                    for j in &ifaces {
                        let jd = &self.structs[j.0 as usize];
                        if let Some(g) = jd.method(name)
                            && !jd.abstract_fns.contains(&g)
                            && !defaults.iter().any(|(_, x)| *x == g)
                        {
                            defaults.push((jd.name.clone(), g));
                        }
                    }
                    match defaults.len() {
                        0 if d.is_class && d.is_abstract => {}
                        0 => errors.push((
                            codes::MISSING_IMPL,
                            format!("`{}` doesn't implement `{}.{name}`", d.name, idef.name),
                        )),
                        1 => {
                            let _ = f;
                        }
                        _ => {
                            let from: Vec<String> =
                                defaults.iter().map(|(n, _)| format!("`{n}`")).collect();
                            errors.push((
                                codes::CONFLICTING_DEFAULTS,
                                format!(
                                    "`{}` inherits conflicting defaults for `{name}` from {}; override it",
                                    d.name,
                                    from.join(" and ")
                                ),
                            ));
                        }
                    }
                }
            }
            let span = self.structs[i].name_span;
            for (code, msg) in errors {
                self.error(code, span, msg, "declared here");
            }
        }
    }

    /// Copies what a subclass inherits: fields (assigned by the base constructor), methods,
    /// accessors, the vtable, and the visibility of each member.
    fn inherit(&mut self, sid: StructId, base: StructId) {
        let b = &self.structs[base.0 as usize];
        let fields: Vec<FieldDef> = b
            .fields
            .iter()
            .map(|f| FieldDef {
                name: f.name.clone(),
                ty: f.ty.clone(),
                span: f.span,
                preassigned: true,
                readonly: false,
            })
            .collect();
        let methods = b.methods.clone();
        let getters = b.getters.clone();
        let setters = b.setters.clone();
        let vtable = b.vtable.clone();
        let slots = b.virtual_slots.clone();
        let private = b.private.clone();
        let protected = b.protected.clone();
        let abstract_fns = b.abstract_fns.clone();
        let final_methods = b.final_methods.clone();
        let mut declared_in = b.declared_in.clone();
        let names = fields
            .iter()
            .map(|f| f.name.clone())
            .chain(methods.iter().map(|(n, _)| n.clone()))
            .chain(getters.iter().map(|(n, _)| n.clone()))
            .chain(setters.iter().map(|(n, _)| n.clone()));
        for n in names {
            declared_in.entry(n).or_insert(base);
        }
        let d = &mut self.structs[sid.0 as usize];
        d.fields = fields;
        d.methods = methods;
        d.getters = getters;
        d.setters = setters;
        d.vtable = vtable;
        d.virtual_slots = slots;
        d.private = private;
        d.protected = protected;
        d.abstract_fns = abstract_fns;
        d.final_methods = final_methods;
        d.declared_in = declared_in;
    }

    /// Pass 1b for a class: fields (and parameter properties), methods, accessors, statics,
    /// constructors and the vtable.
    fn class_members<'a>(&mut self, sid: StructId, cd: &'a ast::ClassDecl) -> ClassInfo<'a> {
        let generics = self.type_params(&cd.generics, &[], Ty::type_owner(sid));
        self.structs[sid.0 as usize].generics = generics;
        if let Some(g) = cd.generics.first()
            && (cd.extends.is_some() || !cd.implements.is_empty())
        {
            self.not_implemented(g.span, "generic classes with `extends` or `implements`");
        }
        let base = self.structs[sid.0 as usize].base;
        if let Some(b) = base {
            if self.structs[b.0 as usize].is_final {
                let bname = self.structs[b.0 as usize].name.clone();
                let span = cd.extends.as_ref().map_or(cd.name.span, |t| t.span);
                self.error(
                    codes::BAD_OVERRIDE,
                    span,
                    format!("`{bname}` is `final` and can't be extended"),
                    "extends a final class",
                );
            }
            self.inherit(sid, b);
        }
        {
            let d = &mut self.structs[sid.0 as usize];
            d.is_abstract = cd.abstract_;
            d.is_final = cd.final_;
        }
        self.resolve_implements(sid, &cd.implements);
        let mut info = ClassInfo {
            sid,
            decl: cd,
            ctors: Vec::new(),
            static_init: None,
            methods: Vec::new(),
            field_inits: Vec::new(),
            static_inits: Vec::new(),
            static_blocks: Vec::new(),
        };
        let mut seen: HashMap<String, Span> = HashMap::new();
        let inherited_fields: HashSet<String> = self.structs[sid.0 as usize]
            .fields
            .iter()
            .map(|f| f.name.clone())
            .collect();
        let class_name = cd.name.name.clone();
        let set_vis = |r: &mut Resolver, name: &str, m: &ast::Modifiers| {
            let d = &mut r.structs[sid.0 as usize];
            d.private.remove(name);
            d.protected.remove(name);
            d.declared_in.remove(name);
            match m.visibility {
                Some(ast::Visibility::Public) => {}
                Some(ast::Visibility::Protected) => {
                    d.protected.insert(name.to_string());
                }
                _ => {
                    d.private.insert(name.to_string());
                }
            }
        };
        let field_clash = |r: &mut Resolver, name: &ast::Ident| {
            if inherited_fields.contains(&name.name) {
                r.error(
                    codes::DUPLICATE_DECL,
                    name.span,
                    format!("`{}` is already a field of a base class", name.name),
                    "declared again here",
                );
                return true;
            }
            false
        };
        for m in &cd.members {
            if let Some(dec) = m.decorators.first() {
                self.not_implemented(dec.span, "decorators");
            }
            if m.mods.is_async {
                self.not_implemented(m.span, "async methods");
            }
            match &m.kind {
                ast::MemberKind::Constructor(f) => {
                    // Parameter properties declare fields.
                    let mut tys = Vec::new();
                    for p in &f.params {
                        let ty = match &p.ty {
                            Some(t) => self.resolve_type(t),
                            None => Ty::Error,
                        };
                        tys.push(ty.clone());
                        if p.mods.visibility.is_none() && !p.mods.readonly {
                            continue;
                        }
                        let ast::PatternKind::Ident(pname) = &p.pat.kind else {
                            continue;
                        };
                        if field_clash(self, pname) || self.check_duplicate(&mut seen, pname) {
                            continue;
                        }
                        set_vis(self, &pname.name, &p.mods);
                        self.structs[sid.0 as usize].fields.push(FieldDef {
                            name: pname.name.clone(),
                            ty,
                            span: pname.span,
                            preassigned: true,
                            readonly: p.mods.readonly,
                        });
                    }
                    let def = &self.structs[sid.0 as usize];
                    if def.ctors.iter().any(|(_, ps)| ps.len() == tys.len()) {
                        self.error(
                            codes::DUPLICATE_DECL,
                            m.span,
                            format!(
                                "`{class_name}` already has a constructor with {} parameter{}",
                                tys.len(),
                                if tys.len() == 1 { "" } else { "s" }
                            ),
                            "overloaded constructors need different numbers of parameters",
                        );
                        continue;
                    }
                    let id = FnId(self.fns.len() as u32);
                    self.fns.push(None);
                    self.structs[sid.0 as usize].ctors.push((id, tys));
                    info.ctors.push((id, Some(f)));
                }
                ast::MemberKind::Field {
                    name,
                    ty,
                    optional,
                    init,
                } => {
                    if field_clash(self, name) || self.check_duplicate(&mut seen, name) {
                        continue;
                    }
                    let mut t = match (ty, init) {
                        (Some(t), _) => Some(self.resolve_type(t)),
                        (None, Some(e)) => literal_type(e),
                        (None, None) => None,
                    };
                    if *optional {
                        t = t.map(Ty::nullable);
                    }
                    if m.mods.is_static {
                        if init.is_none() && !matches!(t, Some(Ty::Nullable(_))) {
                            self.error(
                                codes::MISSING_PARAM_TYPE,
                                name.span,
                                format!("static field `{}` needs an initializer", name.name),
                                "add `= value`",
                            );
                        }
                        let g = GlobalId(self.globals.len() as u32);
                        self.globals.push(Binding {
                            name: format!("{}.{}", cd.name.name, name.name),
                            mutable: !m.mods.readonly,
                            ty: t,
                            span: name.span,
                            boxed: false,
                        });
                        // Initialized before any top-level code runs.
                        self.globals_declared.insert(g);
                        self.structs[sid.0 as usize]
                            .static_fields
                            .push((name.name.clone(), g));
                        set_vis(self, &name.name, &m.mods);
                        info.static_inits.push((g, name, init.as_ref()));
                        continue;
                    }
                    let ty = match t {
                        Some(t) => t,
                        None => {
                            self.error(
                                codes::MISSING_PARAM_TYPE,
                                name.span,
                                format!("field `{}` needs a type", name.name),
                                "add a type annotation, e.g. `: i32`",
                            );
                            Ty::Error
                        }
                    };
                    let nullable = matches!(ty, Ty::Nullable(_));
                    set_vis(self, &name.name, &m.mods);
                    self.structs[sid.0 as usize].fields.push(FieldDef {
                        name: name.name.clone(),
                        ty,
                        span: name.span,
                        preassigned: init.is_some() || nullable,
                        readonly: m.mods.readonly,
                    });
                    info.field_inits.push((name, init.as_ref()));
                }
                ast::MemberKind::Method(f)
                | ast::MemberKind::Getter(f)
                | ast::MemberKind::Setter(f) => {
                    let Some(name) = &f.name else { continue };
                    let is_method = matches!(m.kind, ast::MemberKind::Method(_));
                    // A getter and a setter may share a name.
                    let key = match &m.kind {
                        ast::MemberKind::Setter(_) => format!("set {}", name.name),
                        _ => name.name.clone(),
                    };
                    let k = ast::Ident {
                        name: key,
                        span: name.span,
                    };
                    if self.check_duplicate(&mut seen, &k) {
                        continue;
                    }
                    if !is_method && (m.mods.virtual_ || m.mods.override_ || m.mods.abstract_) {
                        self.not_implemented(m.span, "virtual accessors");
                    }
                    let id = FnId(self.fns.len() as u32);
                    self.fns.push(None);
                    if is_method && !m.mods.is_static {
                        // Before `set_vis`, which forgets where an inherited name came from.
                        self.method_slot(sid, name, &m.mods, id, m.span);
                    }
                    set_vis(self, &name.name, &m.mods);
                    if m.mods.is_static {
                        if !is_method {
                            self.not_implemented(m.span, "static accessors");
                        }
                        self.structs[sid.0 as usize]
                            .static_methods
                            .push((name.name.clone(), id));
                        info.methods.push((id, f, None, false));
                        continue;
                    }
                    let is_abstract = m.mods.abstract_;
                    if m.mods.readonly && is_method {
                        self.structs[sid.0 as usize]
                            .readonly_methods
                            .insert(name.name.clone());
                    }
                    let d = &mut self.structs[sid.0 as usize];
                    let list = match &m.kind {
                        ast::MemberKind::Getter(_) => &mut d.getters,
                        ast::MemberKind::Setter(_) => &mut d.setters,
                        _ => &mut d.methods,
                    };
                    list.retain(|(n, _)| *n != name.name);
                    list.push((name.name.clone(), id));
                    if is_abstract {
                        d.abstract_fns.insert(id);
                    }
                    info.methods.push((id, f, Some(sid), is_abstract));
                }
                ast::MemberKind::StaticBlock(b) => info.static_blocks.push(b),
                ast::MemberKind::Operator { op, func } => {
                    let (name, decl) = operator_method(op, func, m.span);
                    let id = FnId(self.fns.len() as u32);
                    self.fns.push(None);
                    self.structs[sid.0 as usize].methods.push((name, id));
                    info.methods.push((id, decl, Some(sid), false));
                }
            }
        }
        if info.ctors.is_empty() {
            // Without a constructor, a class gets one that takes no arguments, or (in a
            // subclass) one forwarding to each base constructor.
            let shapes: Vec<Vec<Ty>> = match base {
                Some(b) => self.structs[b.0 as usize]
                    .ctors
                    .iter()
                    .map(|(_, ps)| ps.clone())
                    .collect(),
                None => vec![Vec::new()],
            };
            for params in shapes {
                let id = FnId(self.fns.len() as u32);
                self.fns.push(None);
                self.structs[sid.0 as usize].ctors.push((id, params));
                info.ctors.push((id, None));
            }
        }
        let d = &self.structs[sid.0 as usize];
        if !d.is_abstract {
            let missing: Vec<String> = d
                .methods
                .iter()
                .filter(|(_, f)| d.abstract_fns.contains(f))
                .map(|(n, _)| format!("`{n}`"))
                .collect();
            if !missing.is_empty() {
                self.error(
                    codes::ABSTRACT_NOT_IMPLEMENTED,
                    cd.name.span,
                    format!("`{class_name}` doesn't implement {}", missing.join(", ")),
                    "make the class `abstract`, or override the abstract methods",
                );
            }
        }
        if !info.static_inits.is_empty() || !info.static_blocks.is_empty() {
            let id = FnId(self.fns.len() as u32);
            self.fns.push(None);
            info.static_init = Some(id);
        }
        info
    }

    /// Puts method `name` (function `id`) into the class's vtable, enforcing that methods are
    /// final by default and that overrides say `override` (spec §6.1).
    fn method_slot(
        &mut self,
        sid: StructId,
        name: &ast::Ident,
        mods: &ast::Modifiers,
        id: FnId,
        span: Span,
    ) {
        let d = &self.structs[sid.0 as usize];
        let inherited = d.declared_in.get(&name.name).copied();
        let slot = d.virtual_slot(&name.name);
        let class_name = d.name.clone();
        let wants_slot = mods.virtual_ || mods.abstract_;
        if mods.abstract_ && !d.is_abstract {
            self.error(
                codes::ABSTRACT_NOT_IMPLEMENTED,
                span,
                format!(
                    "abstract method `{}` in a class that isn't abstract",
                    name.name
                ),
                format!("declare `abstract class {class_name}`"),
            );
        }
        match (inherited, slot) {
            (Some(b), Some(slot)) => {
                let bname = self.structs[b.0 as usize].name.clone();
                if !mods.override_ {
                    self.diags.push(
                        Diagnostic::error(
                            codes::MISSING_OVERRIDE,
                            format!(
                                "`{}` overrides `{bname}.{}` without `override`",
                                name.name, name.name
                            ),
                        )
                        .primary(name.span, "add `override`")
                        .help(format!("write `override {}(…)`", name.name)),
                    );
                }
                if self.structs[sid.0 as usize]
                    .final_methods
                    .contains(&name.name)
                {
                    self.error(
                        codes::BAD_OVERRIDE,
                        name.span,
                        format!("`{bname}.{}` is `final`", name.name),
                        "can't be overridden",
                    );
                }
                let d = &mut self.structs[sid.0 as usize];
                d.vtable[slot as usize] = id;
            }
            (Some(b), None) => {
                let bname = self.structs[b.0 as usize].name.clone();
                self.diags.push(
                    Diagnostic::error(
                        codes::BAD_OVERRIDE,
                        format!("`{bname}.{}` can't be overridden", name.name),
                    )
                    .primary(name.span, "redefines a method of a base class")
                    .note("methods are final by default")
                    .help(format!(
                        "declare `{bname}.{}` as `virtual` to allow overriding it",
                        name.name
                    )),
                );
            }
            (None, _) => {
                if mods.override_ {
                    self.error(
                        codes::BAD_OVERRIDE,
                        name.span,
                        format!("`{}` has nothing to override", name.name),
                        "no base class method with this name",
                    );
                }
                if wants_slot || mods.override_ {
                    let d = &mut self.structs[sid.0 as usize];
                    let slot = d.vtable.len() as u32;
                    d.vtable.push(id);
                    d.virtual_slots.push((name.name.clone(), slot));
                }
            }
        }
        if mods.final_ {
            self.structs[sid.0 as usize]
                .final_methods
                .insert(name.name.clone());
        }
    }

    /// A class's constructor. In order: `super(…)` (explicit or implied) or `this(…)`, then
    /// (unless delegating with `this(…)`) parameter properties and field initializers in
    /// declaration order, then the rest of the body.
    fn constructor(&mut self, c: &ClassInfo, id: FnId, decl: Option<&ast::FnDecl>) -> Function {
        let sid = c.sid;
        let class_name = self.structs[sid.0 as usize].name.clone();
        let base = self.structs[sid.0 as usize].base;
        let span = decl.map_or(c.decl.name.span, |f| f.span);
        self.f = FnState {
            owner: Some(sid),
            ..Default::default()
        };
        self.enter_type_params(sid);
        self.f.scopes.push(Scope::default());
        let this_ty = self.struct_ty(sid);
        let this = LocalId(self.f.locals.len() as u32);
        self.f.locals.push(Binding {
            name: "this".into(),
            mutable: false,
            ty: Some(this_ty),
            span,
            boxed: false,
        });
        self.f.this = Some((this, sid));
        let mut params = Vec::new();
        let mut props = Vec::new();
        match decl {
            Some(f) => {
                for p in &f.params {
                    let ast::PatternKind::Ident(pname) = &p.pat.kind else {
                        self.not_implemented(p.pat.span, "destructured parameters");
                        continue;
                    };
                    if p.rest || p.optional || p.default.is_some() || p.comptime {
                        self.not_implemented(
                            p.span,
                            "optional, default, rest and comptime parameters",
                        );
                    }
                    let ty = match &p.ty {
                        Some(t) => Some(self.resolve_type(t)),
                        None => {
                            self.error(
                                codes::MISSING_PARAM_TYPE,
                                pname.span,
                                format!("parameter `{}` needs a type", pname.name),
                                "add a type annotation, e.g. `: i32`",
                            );
                            Some(Ty::Error)
                        }
                    };
                    let pid = self.declare_local(pname, true, ty);
                    params.push(pid);
                    if p.mods.visibility.is_some() || p.mods.readonly {
                        props.push((pid, pname));
                    }
                }
            }
            None => {
                // The default constructor forwards its parameters to the base's.
                let tys = self.structs[sid.0 as usize]
                    .ctors
                    .iter()
                    .find(|(f, _)| *f == id)
                    .map(|(_, t)| t.clone())
                    .unwrap_or_default();
                for (i, t) in tys.into_iter().enumerate() {
                    let pid = self.hidden_local(&format!("<arg{i}>"), span);
                    self.f.locals[pid.0 as usize].ty = Some(t);
                    params.push(pid);
                }
            }
        }
        // Split off a leading `super(…)` / `this(…)`.
        let (lead, rest): (Option<&ast::Stmt>, &[ast::Stmt]) =
            match decl.and_then(|f| f.body.as_ref()) {
                Some(ast::FnBody::Block(b)) => match b.stmts.first() {
                    Some(s) if ctor_call_kind(s).is_some() => (Some(s), &b.stmts[1..]),
                    _ => (None, &b.stmts[..]),
                },
                _ => (None, &[]),
            };
        let mut stmts = Vec::new();
        let mut delegates = false;
        match lead {
            Some(s) => {
                let (is_super, args, cspan) = ctor_call_kind(s).expect("checked");
                let target = if is_super { base } else { Some(sid) };
                match target {
                    Some(class) => {
                        delegates = !is_super;
                        let args = args.iter().map(|a| self.lower_expr(&a.value)).collect();
                        let call = self.expr(ExprKind::CtorCall { class, args }, cspan);
                        stmts.push(Stmt::Expr(call));
                    }
                    None => self.error(
                        codes::BAD_CTOR_CALL,
                        cspan,
                        format!("`{class_name}` has no base class"),
                        "`super(…)` without `extends`",
                    ),
                }
            }
            None => {
                if let Some(b) = base {
                    // Implied `super(…)`: the default constructor forwards its arguments.
                    let args: Vec<Expr> = if decl.is_none() {
                        params
                            .iter()
                            .map(|p| self.expr(ExprKind::Var(Var::Local(*p)), span))
                            .collect()
                    } else {
                        Vec::new()
                    };
                    let n = args.len();
                    if self.structs[b.0 as usize].ctor_with(n).is_none() {
                        let bname = self.structs[b.0 as usize].name.clone();
                        self.error(
                            codes::BAD_CTOR_CALL,
                            span,
                            "this constructor must call `super(…)` first",
                            format!("`{bname}` has no constructor without arguments"),
                        );
                    } else {
                        let call = self.expr(ExprKind::CtorCall { class: b, args }, span);
                        stmts.push(Stmt::Expr(call));
                    }
                }
            }
        }
        if !delegates {
            for (pid, pname) in props {
                let value = self.expr(ExprKind::Var(Var::Local(pid)), pname.span);
                let assign = self.field_assign(this, &pname.name, pname.span, value);
                stmts.push(Stmt::Expr(assign));
            }
            for (name, init) in &c.field_inits {
                let value = match init {
                    Some(e) => self.lower_expr(e),
                    None => {
                        let nullable = self.structs[sid.0 as usize]
                            .field(&name.name)
                            .is_some_and(|(_, f)| matches!(f.ty, Ty::Nullable(_)));
                        if !nullable {
                            continue;
                        }
                        self.expr(ExprKind::Null, name.span)
                    }
                };
                let assign = self.field_assign(this, &name.name, name.span, value);
                stmts.push(Stmt::Expr(assign));
            }
        }
        let body = match decl.and_then(|f| f.body.as_ref()) {
            Some(ast::FnBody::Block(b)) => self.with_scope(rest, |r| r.block_stmts(rest, b.span)),
            Some(ast::FnBody::Expr(e)) => {
                let e = self.lower_expr(e);
                Block {
                    stmts: vec![Stmt::Expr(e)],
                    tail: None,
                    span,
                }
            }
            None => Block {
                stmts: Vec::new(),
                tail: None,
                span,
            },
        };
        let mut body = body;
        stmts.append(&mut body.stmts);
        body.stmts = stmts;
        let throws = match decl.and_then(|d| d.throws.as_ref()) {
            Some(t) => self.throws_clause(t),
            None => Vec::new(),
        };
        Function {
            name: format!("{class_name}.constructor"),
            name_span: c.decl.name.span,
            params,
            ret: Some(Ty::Void),
            ret_span: None,
            locals: std::mem::take(&mut self.f.locals),
            body,
            span,
            is_entry: false,
            this: self.f.this,
            owner: Some(sid),
            generics: Vec::new(),
            closure: None,
            defaults: Vec::new(),
            rest: false,
            throws,
            infer_throws: false,
        }
    }

    /// The function for an abstract method: it panics (it can only be reached through a
    /// broken vtable, since abstract classes can't be instantiated).
    fn abstract_stub(&mut self, f: &ast::FnDecl, sid: StructId) -> Function {
        let name = f.name.clone().expect("named method");
        let span = name.span;
        let mut func = self.function_in(
            &ast::FnDecl {
                body: Some(ast::FnBody::Block(ast::Block {
                    stmts: Vec::new(),
                    span,
                })),
                ..f.clone()
            },
            false,
            Some(sid),
            Some(sid),
        );
        let msg = self.expr(
            ExprKind::Str(format!("abstract method `{}` called", name.name)),
            span,
        );
        let callee = self.expr(ExprKind::Builtin(Builtin::Panic), span);
        let call = self.expr(
            ExprKind::Call {
                callee: Box::new(callee),
                args: vec![msg],
                type_args: Vec::new(),
            },
            span,
        );
        func.body.tail = Some(Box::new(call));
        func
    }

    /// `this.name = value`
    fn field_assign(&mut self, this: LocalId, name: &str, span: Span, value: Expr) -> Expr {
        let object = self.expr(ExprKind::Var(Var::Local(this)), span);
        let id = self.expr(ExprKind::Error, span).id;
        self.expr(
            ExprKind::Assign {
                op: None,
                target: Target::Field {
                    id,
                    object: Box::new(object),
                    name: name.to_string(),
                    name_span: span,
                },
                value: Box::new(value),
            },
            span,
        )
    }

    /// Static field initializers and `static { }` blocks, in declaration order.
    fn static_init(&mut self, c: &ClassInfo) -> Function {
        let sid = c.sid;
        let class_name = self.structs[sid.0 as usize].name.clone();
        self.f = FnState {
            owner: Some(sid),
            ..Default::default()
        };
        self.f.scopes.push(Scope::default());
        let mut stmts = Vec::new();
        for (g, name, init) in &c.static_inits {
            let init = match init {
                Some(e) => self.lower_expr(e),
                None => self.expr(ExprKind::Null, name.span),
            };
            stmts.push(Stmt::Let {
                var: Var::Global(*g),
                init: Some(init),
                span: name.span,
            });
        }
        for b in &c.static_blocks {
            let block = self.block(b);
            stmts.push(Stmt::Expr(self.expr(ExprKind::Block(block), b.span)));
        }
        Function {
            name: format!("{class_name}.<static>"),
            name_span: c.decl.name.span,
            params: Vec::new(),
            ret: Some(Ty::Void),
            ret_span: None,
            locals: std::mem::take(&mut self.f.locals),
            body: Block {
                stmts,
                tail: None,
                span: c.decl.name.span,
            },
            span: c.decl.name.span,
            is_entry: false,
            this: None,
            owner: Some(sid),
            generics: Vec::new(),
            closure: None,
            defaults: Vec::new(),
            rest: false,
            throws: Vec::new(),
            infer_throws: false,
        }
    }

    fn struct_ty(&self, id: StructId) -> Ty {
        self.structs[id.0 as usize].self_ty(id)
    }

    // ------------------------------------------------------------ scopes

    fn declare_local(&mut self, name: &ast::Ident, mutable: bool, ty: Option<Ty>) -> LocalId {
        let id = LocalId(self.f.locals.len() as u32);
        self.f.locals.push(Binding {
            name: name.name.clone(),
            mutable,
            ty,
            span: name.span,
            boxed: false,
        });
        let scope = self.f.scopes.last_mut().expect("a scope");
        scope.later.remove(&name.name);
        scope.names.insert(name.name.clone(), id);
        id
    }

    /// A `let`/`const` local, with an error if the block already declares the name.
    fn declare_block_local(&mut self, name: &ast::Ident, mutable: bool, ty: Option<Ty>) -> LocalId {
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
        self.declare_local(name, mutable, ty)
    }

    /// `const [a, [b, _]] = source` → a hidden local holding `source`, then one binding per
    /// name, initialized from `hidden[i]`. At the top level (`global`) the names are globals.
    fn destructure(
        &mut self,
        pat: &ast::Pattern,
        source: Expr,
        mutable: bool,
        ty: Option<Ty>,
        out: &mut Vec<Stmt>,
        global: bool,
    ) {
        let span = pat.span;
        match &pat.kind {
            ast::PatternKind::Ident(name) => {
                if global {
                    match self.top_globals.get(&name.name).copied() {
                        Some(g) if self.globals[g.0 as usize].span == name.span => {
                            self.globals_declared.insert(g);
                            out.push(Stmt::Let {
                                var: Var::Global(g),
                                init: Some(source),
                                span,
                            });
                        }
                        _ => out.push(Stmt::Expr(source)),
                    }
                } else {
                    let id = self.declare_block_local(name, mutable, ty);
                    out.push(Stmt::Let {
                        var: Var::Local(id),
                        init: Some(source),
                        span,
                    });
                }
            }
            ast::PatternKind::Wildcard => out.push(Stmt::Expr(source)),
            ast::PatternKind::Array(elems) => {
                let tmp = self.hidden_local("<destructured>", span);
                self.f.locals[tmp.0 as usize].ty = ty;
                out.push(Stmt::Let {
                    var: Var::Local(tmp),
                    init: Some(source),
                    span,
                });
                for (i, sub) in elems.iter().enumerate() {
                    if let ast::PatternKind::Rest(_) = sub.kind {
                        self.not_implemented(sub.span, "rest patterns (`...rest`)");
                        continue;
                    }
                    let object = self.expr(ExprKind::Var(Var::Local(tmp)), sub.span);
                    let index = self.expr(ExprKind::Int(i as u64, None), sub.span);
                    let elem = self.expr(
                        ExprKind::Index {
                            object: Box::new(object),
                            index: Box::new(index),
                        },
                        sub.span,
                    );
                    self.destructure(sub, elem, mutable, None, out, global);
                }
            }
            _ => self.not_implemented(span, "this kind of destructuring pattern"),
        }
    }

    fn destructure_without_init(&mut self, pat: &ast::Pattern) {
        self.diags.push(
            Diagnostic::error(
                codes::MISMATCHED_TYPES,
                "a destructuring declaration needs a value",
            )
            .primary(pat.span, "nothing to destructure")
            .help("add an initializer: `const [a, b] = pair`"),
        );
    }

    /// A compiler-generated local (loop counters).
    fn hidden_local(&mut self, name: &str, span: Span) -> LocalId {
        let id = LocalId(self.f.locals.len() as u32);
        self.f.locals.push(Binding {
            name: name.into(),
            mutable: true,
            ty: None,
            span,
            boxed: false,
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
        // A closure sees the locals of the functions around it (capturing them).
        if let Some(id) = self.capture(name) {
            return Some(Found::Var(Var::Local(id)));
        }
        if self.f.this.is_none() && self.f.arrow {
            self.capture_this();
        }
        // Inside a method, `name` means `this.name` unless a local shadows it.
        if let Some((_, sid)) = self.f.this {
            let def = &self.structs[sid.0 as usize];
            if def.field(name).is_some()
                || def.method(name).is_some()
                || def.getter(name).is_some()
                || def.setter(name).is_some()
            {
                return Some(Found::Member(name.to_string()));
            }
        }
        // Inside a class, its static members by their plain names.
        if let Some(sid) = self.f.owner {
            let def = &self.structs[sid.0 as usize];
            if let Some(g) = def.static_field(name) {
                return Some(Found::Var(Var::Global(g)));
            }
            if let Some(f) = def.static_method(name) {
                return Some(Found::Fn(f));
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
        if let Some(fs) = self.top_fns.get(name) {
            return Some(match fs.as_slice() {
                [f] => Found::Fn(*f),
                many => Found::Overloads(many.to_vec()),
            });
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

    /// The function state `level` steps out (0 = the current one).
    fn level(&mut self, level: usize) -> &mut FnState {
        if level == 0 {
            &mut self.f
        } else {
            let n = self.outer_fns.len();
            &mut self.outer_fns[n - level]
        }
    }

    /// Finds `name` among the locals of enclosing functions and captures it into every
    /// closure in between: a `const` by value, a `let` by reference (boxing the variable).
    fn capture(&mut self, name: &str) -> Option<LocalId> {
        if !self.f.is_closure {
            return None;
        }
        let depth = self.outer_fns.len();
        let mut found = None;
        for lvl in 1..=depth {
            let st = self.level(lvl);
            if let Some(id) = st
                .scopes
                .iter()
                .rev()
                .find_map(|sc| sc.names.get(name).copied())
            {
                found = Some((lvl, id));
                break;
            }
            // Only closures see outward; a named function's state ends the search.
            if !st.is_closure {
                break;
            }
            if lvl == depth {
                break;
            }
        }
        let (lvl, mut id) = found?;
        // Closures between the declaring function and here can only see outward if they are
        // closures themselves (checked above for the levels searched).
        for l in (0..lvl).rev() {
            let outer_binding = self.level(l + 1).locals[id.0 as usize].clone();
            let by_ref = outer_binding.mutable || outer_binding.boxed;
            if by_ref {
                self.level(l + 1).locals[id.0 as usize].boxed = true;
            }
            let st = self.level(l);
            let inner = LocalId(st.locals.len() as u32);
            st.locals.push(Binding {
                name: name.to_string(),
                mutable: outer_binding.mutable,
                ty: outer_binding.ty.clone(),
                span: outer_binding.span,
                boxed: by_ref,
            });
            st.captures.push(Capture {
                outer: id,
                inner,
                by_ref,
            });
            if let Some(top) = st.scopes.first_mut() {
                top.names.insert(name.to_string(), inner);
            }
            id = inner;
        }
        Some(id)
    }

    /// `this` here: the method's, or (in an arrow) the enclosing method's, captured.
    fn this_here(&mut self) -> Option<(LocalId, StructId)> {
        self.capture_this();
        self.f.this
    }

    /// An arrow function uses `this` (and implicit members) of the method around it.
    fn capture_this(&mut self) {
        if self.f.this.is_some() || !self.f.arrow {
            return;
        }
        let depth = self.outer_fns.len();
        let mut found = None;
        for lvl in 1..=depth {
            let st = self.level(lvl);
            if let Some(t) = st.this {
                found = Some((lvl, t));
                break;
            }
            if !(st.is_closure && st.arrow) {
                return;
            }
        }
        let Some((lvl, (mut id, sid))) = found else {
            return;
        };
        for l in (0..lvl).rev() {
            let outer_binding = self.level(l + 1).locals[id.0 as usize].clone();
            let st = self.level(l);
            let inner = LocalId(st.locals.len() as u32);
            st.locals.push(outer_binding);
            st.captures.push(Capture {
                outer: id,
                inner,
                by_ref: false,
            });
            st.this = Some((inner, sid));
            st.owner = st.owner.or(Some(sid));
            id = inner;
        }
    }

    /// An arrow function or function expression: lifted into its own function that receives
    /// the captured variables.
    fn closure(&mut self, f: &ast::FnDecl, span: Span) -> ExprKind {
        let id = FnId(self.fns.len() as u32);
        self.fns.push(None);
        let parent = self.cur_fn;
        let outer = std::mem::take(&mut self.f);
        let owner = outer.owner;
        let in_entry = outer.in_entry;
        self.outer_fns.push(outer);
        self.f = FnState {
            is_closure: true,
            arrow: f.flags.arrow,
            owner: if f.flags.arrow { owner } else { None },
            ..Default::default()
        };
        let _ = in_entry;
        // Type parameters of the functions around it stay visible.
        let tps: Vec<(String, Ty)> = self
            .outer_fns
            .iter()
            .flat_map(|o| o.type_params.clone())
            .collect();
        self.f.type_params = tps;
        self.f.scopes.push(Scope::default());
        if f.flags.is_async || f.flags.generator {
            self.not_implemented(span, "async and generator closures");
        }
        if let Some(g) = f.generics.first() {
            self.not_implemented(g.span, "generic closures");
        }
        let mut params = Vec::new();
        for p in &f.params {
            let ast::PatternKind::Ident(pname) = &p.pat.kind else {
                self.not_implemented(p.pat.span, "destructured parameters");
                continue;
            };
            if p.rest || p.optional || p.default.is_some() || p.comptime {
                self.not_implemented(p.span, "optional, default, rest and comptime parameters");
            }
            // An arrow's parameter types may come from the expected function type.
            let ty = p.ty.as_ref().map(|t| self.resolve_type(t));
            params.push(self.declare_local(pname, true, ty));
        }
        let ret = f.ret.as_ref().map(|t| self.resolve_type(t));
        let saved_fn = self.cur_fn;
        self.cur_fn = id;
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
            None => Block {
                stmts: Vec::new(),
                tail: None,
                span,
            },
        };
        self.cur_fn = saved_fn;
        let st = std::mem::take(&mut self.f);
        self.f = self.outer_fns.pop().expect("pushed above");
        let name = match &f.name {
            Some(n) => n.name.clone(),
            None => format!("<closure@{}>", id.0),
        };
        self.fns[id.0 as usize] = Some(Function {
            name,
            name_span: f.name.as_ref().map_or(span.shrink_to_lo(), |n| n.span),
            params,
            ret,
            ret_span: f.ret.as_ref().map(|t| t.span),
            locals: st.locals,
            body,
            span,
            is_entry: false,
            this: st.this,
            owner: st.owner,
            generics: Vec::new(),
            closure: Some(ClosureInfo {
                parent,
                captures: st.captures,
            }),
            defaults: Vec::new(),
            rest: false,
            throws: Vec::new(),
            infer_throws: true,
        });
        ExprKind::Closure(id)
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
        (def.field(name).is_some() || def.setter(name).is_some())
            .then(|| Found::Member(name.to_string()))
    }

    fn resolve_var(&mut self, name: &str, span: Span) -> Option<Var> {
        match self.lookup(name, span)? {
            Found::Var(v) => Some(v),
            Found::Fn(_) | Found::Overloads(_) | Found::Builtin(_) | Found::Member(_) => {
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
            ast::TypeKind::Path { path, args }
                if path.len() == 1
                    && args.is_empty()
                    && self.f.type_params.iter().any(|(n, _)| *n == path[0].name) =>
            {
                let (_, ty) = self
                    .f
                    .type_params
                    .iter()
                    .find(|(n, _)| *n == path[0].name)
                    .expect("checked");
                ty.clone()
            }
            ast::TypeKind::Path { path, args } if path.len() == 1 && args.is_empty() => {
                match Ty::from_name(&path[0].name) {
                    Some(ty) => ty,
                    None if self.top_structs.contains_key(&path[0].name) => {
                        let sid = self.top_structs[&path[0].name];
                        if self.structs[sid.0 as usize].generics.is_empty() {
                            self.struct_ty(sid)
                        } else {
                            self.generic_ty(sid, Vec::new(), t.span)
                        }
                    }
                    None if self.top_enums.contains_key(&path[0].name) => {
                        self.enum_ty(self.top_enums[&path[0].name])
                    }
                    None if self.aliases.contains_key(&path[0].name) => {
                        self.alias(&path[0].name, t.span)
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
            ast::TypeKind::Path { path, args }
                if path.len() == 1 && path[0].name == "Array" && args.len() == 1 =>
            {
                Ty::Array(Box::new(self.resolve_type(&args[0])))
            }
            ast::TypeKind::Path { path, args }
                if path.len() == 1 && self.top_structs.contains_key(&path[0].name) =>
            {
                let sid = self.top_structs[&path[0].name];
                let args: Vec<Ty> = args.iter().map(|a| self.resolve_type(a)).collect();
                self.generic_ty(sid, args, t.span)
            }
            ast::TypeKind::Array(elem) => Ty::Array(Box::new(self.resolve_type(elem))),
            ast::TypeKind::Fixed { elem, len } => {
                let elem = self.resolve_type(elem);
                match self.const_len(len) {
                    Some(n) => Ty::Fixed(Box::new(elem), n),
                    None => Ty::Error,
                }
            }
            ast::TypeKind::Tuple(elems) => {
                Ty::Tuple(elems.iter().map(|t| self.resolve_type(t)).collect())
            }
            ast::TypeKind::Nullable(inner) => self.resolve_type(inner).nullable(),
            ast::TypeKind::Lit(ast::Lit::Null) => Ty::Null,
            ast::TypeKind::Lit(l) => match lit_val(l) {
                Some((base, v)) => Ty::Lits(Box::new(base), vec![v].into()),
                None => {
                    self.not_implemented(t.span, "this kind of literal type");
                    Ty::Error
                }
            },
            ast::TypeKind::Union(members) => self.union_type(members, None, t.span),
            ast::TypeKind::Function {
                params,
                ret,
                throws,
                extern_abi,
            } => {
                if extern_abi.is_some() {
                    self.not_implemented(t.span, "`extern` function types");
                }
                for p in params {
                    if p.optional || p.rest {
                        self.not_implemented(
                            t.span,
                            "optional and rest parameters in function types",
                        );
                    }
                }
                let ps: Vec<Ty> = params.iter().map(|p| self.resolve_type(&p.ty)).collect();
                let ret = self.resolve_type(ret);
                let ts = match throws {
                    Some(th) => self.throws_clause(th),
                    None => Vec::new(),
                };
                Ty::Fn(ps.into(), Box::new(ret), ts.into())
            }
            ast::TypeKind::Object(_) => {
                self.not_implemented(t.span, "object types outside discriminated unions");
                Ty::Error
            }
            ast::TypeKind::Immutable(inner) => Ty::Immutable(Box::new(self.resolve_type(inner))),
            ast::TypeKind::Error => Ty::Error,
            _ => {
                self.not_implemented(t.span, "this kind of type");
                Ty::Error
            }
        }
    }

    /// A struct, class or interface type with type arguments (defaults fill in missing ones).
    fn generic_ty(&mut self, sid: StructId, mut args: Vec<Ty>, span: Span) -> Ty {
        let def = &self.structs[sid.0 as usize];
        let n = def.generics.len();
        if args.len() > n {
            let name = def.name.clone();
            self.error(
                codes::WRONG_ARG_COUNT,
                span,
                format!(
                    "`{name}` takes {n} type argument{}",
                    if n == 1 { "" } else { "s" }
                ),
                "too many type arguments",
            );
            return Ty::Error;
        }
        while args.len() < n {
            match &self.structs[sid.0 as usize].generics[args.len()].default {
                Some(d) => args.push(d.clone()),
                None => {
                    let name = self.structs[sid.0 as usize].name.clone();
                    self.error(
                        codes::CANT_INFER,
                        span,
                        format!("`{name}` needs its type arguments"),
                        format!("write `{name}<…>`"),
                    );
                    return Ty::Error;
                }
            }
        }
        self.structs[sid.0 as usize].ty_with(sid, args.into())
    }

    /// The type an alias stands for (resolved once; a cycle is an error).
    fn alias(&mut self, name: &str, span: Span) -> Ty {
        if let Some(t) = self.alias_cache.get(name) {
            return t.clone();
        }
        if self.resolving_aliases.iter().any(|n| n == name) {
            self.error(
                codes::RECURSIVE_STRUCT,
                span,
                format!("type alias `{name}` refers to itself"),
                "aliases can't be recursive",
            );
            return Ty::Error;
        }
        let (ty, _) = self.aliases[name].clone();
        self.resolving_aliases.push(name.to_string());
        let t = match &ty.kind {
            ast::TypeKind::Union(members) => self.union_type(members, Some(name), ty.span),
            ast::TypeKind::Distinct(base) => {
                let b = self.resolve_type(base);
                self.distincts += 1;
                Ty::Distinct(self.distincts, name.into(), Box::new(b))
            }
            _ => self.resolve_type(&ty),
        };
        self.resolving_aliases.pop();
        self.alias_cache.insert(name.to_string(), t.clone());
        t
    }

    /// `A | B | …`: `T | null` is `T?`; literals of one base type stay that type; object
    /// types with a common literal field become a discriminated enum; anything else is a
    /// tagged union.
    fn union_type(&mut self, members: &[ast::Type], name: Option<&str>, span: Span) -> Ty {
        let objects: Vec<&Vec<ast::ObjectTypeField>> = members
            .iter()
            .filter_map(|m| match &m.kind {
                ast::TypeKind::Object(fs) => Some(fs),
                _ => None,
            })
            .collect();
        if !objects.is_empty() {
            if objects.len() != members.len() {
                self.not_implemented(span, "unions mixing object types with other types");
                return Ty::Error;
            }
            return self.discriminated(&objects, name, span);
        }
        let mut flat: Vec<Ty> = Vec::new();
        let mut nullable = false;
        for m in members {
            let t = self.resolve_type(m);
            let parts: Vec<Ty> = match t {
                Ty::Union(_, ms) => ms.to_vec(),
                Ty::Nullable(inner) => {
                    nullable = true;
                    vec![*inner]
                }
                Ty::Null => {
                    nullable = true;
                    Vec::new()
                }
                Ty::Error => return Ty::Error,
                t => vec![t],
            };
            for p in parts {
                // Literals of one base type merge into one literal type.
                if let Ty::Lits(b, ls) = &p
                    && let Some(Ty::Lits(_, prev)) = flat
                        .iter_mut()
                        .find(|x| matches!(x, Ty::Lits(b2, _) if b2 == b))
                {
                    let mut all = prev.to_vec();
                    for l in ls.iter() {
                        if !all.contains(l) {
                            all.push(l.clone());
                        }
                    }
                    *prev = all.into();
                    continue;
                }
                if !flat.contains(&p) {
                    flat.push(p);
                }
            }
        }
        // A literal type next to its own base type adds nothing: `"a" | string` is `string`.
        let bases: Vec<Ty> = flat
            .iter()
            .filter(|t| !matches!(t, Ty::Lits(..)))
            .cloned()
            .collect();
        flat.retain(|t| !matches!(t, Ty::Lits(b, _) if bases.contains(b)));
        let t = match flat.len() {
            0 => Ty::Null,
            1 => flat.pop().expect("one member"),
            _ => self.intern_union(flat, name),
        };
        if nullable { t.nullable() } else { t }
    }

    fn intern_union(&mut self, members: Vec<Ty>, name: Option<&str>) -> Ty {
        if let Some((_, id)) = self.unions.iter().find(|(m, _)| *m == members) {
            return Ty::Union(*id, members.into());
        }
        let id = EnumId(self.enums.len() as u32);
        let display = members
            .iter()
            .map(|t| t.to_string())
            .collect::<Vec<_>>()
            .join(" | ");
        let span = Span::new(self.file, 0, 0);
        self.enums.push(EnumDef {
            name: name.map_or(display, str::to_string),
            name_span: span,
            variants: members
                .iter()
                .map(|t| VariantDef {
                    name: t.to_string(),
                    span,
                    fields: vec![FieldDef {
                        name: "value".into(),
                        ty: t.clone(),
                        span,
                        preassigned: false,
                        readonly: false,
                    }],
                    has_parens: true,
                })
                .collect(),
            origin: EnumOrigin::Union(members.clone()),
        });
        self.unions.push((members.clone(), id));
        Ty::Union(id, members.into())
    }

    /// `{ kind: "click", x: i32 } | { kind: "key", code: u32 }`: an anonymous enum with a
    /// variant per object type, named by the discriminant (a field with a literal type in
    /// every member); the discriminant is each variant's field 0.
    fn discriminated(
        &mut self,
        objects: &[&Vec<ast::ObjectTypeField>],
        name: Option<&str>,
        span: Span,
    ) -> Ty {
        let disc = objects[0].iter().find(|f| {
            objects.iter().all(|o| {
                o.iter().any(|g| {
                    g.name.name == f.name.name && matches!(g.ty.kind, ast::TypeKind::Lit(_))
                })
            })
        });
        let Some(disc) = disc.map(|f| f.name.name.clone()) else {
            self.error(
                codes::UNKNOWN_TYPE,
                span,
                "a union of object types needs a discriminant",
                "no field has a literal type in every member",
            );
            return Ty::Error;
        };
        let id = EnumId(self.enums.len() as u32);
        let ename = name.map_or_else(|| "<object union>".to_string(), str::to_string);
        self.enums.push(EnumDef {
            name: ename.clone(),
            name_span: span,
            variants: Vec::new(),
            origin: EnumOrigin::Discriminated(disc.clone()),
        });
        let mut seen: HashMap<String, Span> = HashMap::new();
        for o in objects {
            let tag = o.iter().find(|f| f.name.name == disc).expect("checked");
            let ast::TypeKind::Lit(l) = &tag.ty.kind else {
                unreachable!("checked")
            };
            let vname = match l {
                ast::Lit::Str(s) => s.clone(),
                other => format!("{other:?}"),
            };
            let vid = ast::Ident {
                name: vname.clone(),
                span: tag.ty.span,
            };
            if self.check_duplicate(&mut seen, &vid) {
                continue;
            }
            let mut fields = vec![FieldDef {
                name: disc.clone(),
                ty: self.resolve_type(&tag.ty),
                span: tag.name.span,
                preassigned: false,
                readonly: false,
            }];
            for f in o.iter().filter(|f| f.name.name != disc) {
                let mut ty = self.resolve_type(&f.ty);
                if f.optional {
                    ty = ty.nullable();
                }
                fields.push(FieldDef {
                    name: f.name.name.clone(),
                    ty,
                    span: f.name.span,
                    preassigned: f.optional,
                    readonly: false,
                });
            }
            self.enums[id.0 as usize].variants.push(VariantDef {
                name: vname,
                span: tag.ty.span,
                fields,
                has_parens: true,
            });
        }
        Ty::Enum(id, ename.into())
    }

    /// The length of a fixed array: an integer literal (until `comptime` arrives).
    fn const_len(&mut self, e: &ast::Expr) -> Option<u64> {
        match &strip_parens(e).kind {
            ast::ExprKind::Lit(ast::Lit::Int { value, .. }) => Some(*value),
            _ => {
                self.not_implemented(e.span, "array lengths other than integer literals");
                None
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
            if let ast::StmtKind::Let { pat, .. } = &s.kind {
                let mut names = Vec::new();
                pattern_idents(pat, &mut names);
                for n in names {
                    scope.later.entry(n.name.clone()).or_insert(n.span);
                }
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
                self.stmt_switch = matches!(expr.kind, ast::ExprKind::Switch { .. });
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
                let ty = ty.as_ref().map(|t| self.resolve_type(t));
                let ast::PatternKind::Ident(name) = &pat.kind else {
                    match init {
                        Some(e) => {
                            let init = self.lower_expr(e);
                            let mutable = *kind == ast::BindingKind::Let;
                            self.destructure(pat, init, mutable, ty, out, false);
                        }
                        None => self.destructure_without_init(pat),
                    }
                    return;
                };
                let init = init.as_ref().map(|e| self.lower_expr(e));
                let id = self.declare_block_local(name, *kind == ast::BindingKind::Let, ty);
                out.push(Stmt::Let {
                    var: Var::Local(id),
                    init,
                    span: s.span,
                });
            }
            ast::StmtKind::Using { .. } => self.not_implemented(s.span, "`using`"),
            ast::StmtKind::While { cond, body } => {
                self.f.scopes.push(Scope::default());
                self.cond_scope = true;
                let cond = self.lower_expr(cond);
                self.cond_scope = false;
                let body = self.loop_body(body, LoopKind::While);
                self.f.scopes.pop();
                let e = self.expr(
                    ExprKind::While {
                        cond: Box::new(cond),
                        body: Box::new(body),
                        update: None,
                        test_first: true,
                        rebox: Vec::new(),
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
                        rebox: Vec::new(),
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
                kind,
                pat,
                iter,
                body,
            } => {
                let e = self.for_of(*is_await, *kind, pat, iter, body, s.span);
                out.push(Stmt::Expr(e));
            }
            ast::StmtKind::Decl(d) => {
                let what = match &d.kind {
                    ast::DeclKind::Function(_) => "nested functions",
                    ast::DeclKind::Struct(_) => "structs",
                    ast::DeclKind::Enum(_) => "enums",
                    ast::DeclKind::Class(_) => "classes",
                    ast::DeclKind::Interface(_) => "local interfaces",
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
            let first_local = r.f.locals.len();
            let _ = first_local;
            let update = update.map(|u| Box::new(r.lower_expr(u)));
            let body = r.loop_body(body, LoopKind::While);
            // `let` variables of the init that a closure captured get a box per iteration.
            let rebox: Vec<LocalId> = stmts
                .iter()
                .filter_map(|st| match st {
                    Stmt::Let {
                        var: Var::Local(l), ..
                    } if r.f.locals[l.0 as usize].boxed => Some(*l),
                    _ => None,
                })
                .collect();
            let w = r.expr(
                ExprKind::While {
                    cond: Box::new(cond),
                    body: Box::new(body),
                    update,
                    test_first: true,
                    rebox,
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

    /// `for (pat of iter) body`. A destructuring pattern binds a hidden loop variable and
    /// destructures it at the top of the body.
    fn for_of(
        &mut self,
        is_await: bool,
        kind: ast::BindingKind,
        pat: &ast::Pattern,
        iter: &ast::Expr,
        body: &ast::Expr,
        span: Span,
    ) -> Expr {
        if is_await {
            self.not_implemented(span, "`for await`");
        }
        let iter = self.lower_expr(iter);
        let mutable = kind == ast::BindingKind::Let;
        // The body sees a fresh binding of the loop variable each iteration.
        self.f.scopes.push(Scope::default());
        let mut prelude = Vec::new();
        let var = match &pat.kind {
            ast::PatternKind::Ident(name) => self.declare_local(name, mutable, None),
            _ => {
                let v = self.hidden_local("<item>", pat.span);
                let src = self.expr(ExprKind::Var(Var::Local(v)), pat.span);
                self.destructure(pat, src, mutable, None, &mut prelude, false);
                v
            }
        };
        let mut body = self.loop_body(body, LoopKind::While);
        self.f.scopes.pop();
        if !prelude.is_empty() {
            let bspan = body.span;
            prelude.push(Stmt::Expr(body));
            body = self.expr(
                ExprKind::Block(Block {
                    stmts: prelude,
                    tail: None,
                    span: bspan,
                }),
                bspan,
            );
        }
        self.expr(
            ExprKind::ForOf {
                var,
                iter: Box::new(iter),
                body: Box::new(body),
            },
            span,
        )
    }

    // ------------------------------------------------------------ expressions

    fn lower_expr(&mut self, e: &ast::Expr) -> Expr {
        use ast::ExprKind as A;
        let span = e.span;
        if let Some(base) = opt_link_base(e) {
            return self.opt_chain(e, base);
        }
        // Only parentheses and `&&` keep a condition's `is` bindings visible to the body.
        let cond = std::mem::replace(&mut self.cond_scope, false);
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
                Some(Found::Overloads(fs)) => ExprKind::Overloaded(fs),
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
            A::Function(decl) => self.closure(decl, span),
            A::Tailcall(inner) => ExprKind::Tailcall(Box::new(self.lower_expr(inner))),
            A::Throw(inner) => ExprKind::Throw(Box::new(self.lower_expr(inner))),
            A::TryOpt(inner) => ExprKind::TryOpt(Box::new(self.lower_expr(inner))),
            A::TryBang(inner) => ExprKind::TryBang(Box::new(self.lower_expr(inner))),
            A::Try {
                body,
                catches,
                finally,
            } => {
                let body = self.lower_expr(body);
                let catches = catches
                    .iter()
                    .map(|c| {
                        self.f.scopes.push(Scope::default());
                        let mut bound = Binds::default();
                        let pat = self.lower_pattern(&c.pat, &mut bound);
                        let body = self.lower_expr(&c.body);
                        self.f.scopes.pop();
                        CatchClause {
                            pat,
                            body,
                            span: c.span,
                        }
                    })
                    .collect();
                let finally = finally.as_ref().map(|b| {
                    let block = self.block(b);
                    Box::new(self.expr(ExprKind::Block(block), b.span))
                });
                ExprKind::Try {
                    body: Box::new(body),
                    catches,
                    finally,
                }
            }
            A::This => match self.this_here() {
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
            A::Paren(inner) => {
                self.cond_scope = cond;
                return self.lower_expr(inner);
            }
            A::Binary {
                op: ast::BinOp::And,
                lhs,
                rhs,
            } => {
                self.cond_scope = cond;
                let l = self.lower_expr(lhs);
                self.cond_scope = cond;
                let r = self.lower_expr(rhs);
                ExprKind::Binary(ast::BinOp::And, Box::new(l), Box::new(r))
            }
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
                ast::UnOp::Typeof => ExprKind::TypeOf(Box::new(self.lower_expr(expr))),
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
            A::Call { callee, .. }
                if matches!(callee.kind, ast::ExprKind::Super | ast::ExprKind::This) =>
            {
                self.error(
                    codes::BAD_CTOR_CALL,
                    span,
                    "`super(…)` and `this(…)` must be the first statement of a constructor",
                    "not at the start of a constructor",
                );
                ExprKind::Error
            }
            A::Call { callee, args, .. } if self.names_enum_member(callee) => {
                let Some((enum_, variant)) = self.variant_ref(callee) else {
                    return self.error_expr(span);
                };
                let args = args
                    .iter()
                    .map(|a| {
                        if a.spread {
                            self.not_implemented(a.value.span, "spread arguments");
                        }
                        VariantArg {
                            name: a.name.as_ref().map(|n| (n.name.clone(), n.span)),
                            value: self.lower_expr(&a.value),
                        }
                    })
                    .collect();
                ExprKind::Variant {
                    enum_,
                    variant,
                    args: Some(args),
                }
            }
            A::Member { object, prop, .. } if self.names_class(object) => {
                match self.static_member(object, prop) {
                    Some(k) => k,
                    None => ExprKind::Error,
                }
            }
            A::New {
                callee,
                type_args,
                args,
            } => {
                let type_args: Vec<Ty> = type_args.iter().map(|t| self.resolve_type(t)).collect();
                let class = match &callee.kind {
                    ast::ExprKind::Ident(n) => self
                        .top_structs
                        .get(n)
                        .copied()
                        .filter(|id| self.structs[id.0 as usize].is_class),
                    _ => None,
                };
                let Some(class) = class else {
                    self.error(
                        codes::UNKNOWN_TYPE,
                        callee.span,
                        "`new` needs a class",
                        "not a class",
                    );
                    return self.error_expr(span);
                };
                let out = self.call_args(args);
                if self.structs[class.0 as usize].is_abstract {
                    let name = self.structs[class.0 as usize].name.clone();
                    self.error(
                        codes::ABSTRACT_NEW,
                        callee.span,
                        format!("`{name}` is abstract"),
                        "abstract classes can't be instantiated",
                    );
                }
                ExprKind::New {
                    class,
                    args: out,
                    type_args,
                }
            }
            A::Instanceof { expr, ty } => {
                let e2 = self.lower_expr(expr);
                match self.resolve_type(ty) {
                    Ty::Class(class, _, _) => ExprKind::InstanceOf {
                        expr: Box::new(e2),
                        class,
                    },
                    Ty::Error => ExprKind::Error,
                    other => {
                        self.error(
                            codes::UNKNOWN_TYPE,
                            ty.span,
                            format!("`instanceof` needs a class, not `{other}`"),
                            "not a class",
                        );
                        ExprKind::Error
                    }
                }
            }
            A::Call { callee, args, .. } if matches!(&callee.kind, ast::ExprKind::Member { object, .. } if matches!(object.kind, ast::ExprKind::Super)) =>
            {
                let ast::ExprKind::Member { prop, .. } = &callee.kind else {
                    unreachable!()
                };
                let base = self
                    .f
                    .this
                    .and_then(|(_, sid)| self.structs[sid.0 as usize].base);
                let method = base.and_then(|b| self.structs[b.0 as usize].method(&prop.name));
                match method {
                    Some(m)
                        if !self.structs[base.unwrap().0 as usize]
                            .abstract_fns
                            .contains(&m) =>
                    {
                        let args = args.iter().map(|a| self.lower_expr(&a.value)).collect();
                        ExprKind::SuperCall { method: m, args }
                    }
                    _ => {
                        self.error(
                            codes::UNKNOWN_MEMBER,
                            prop.span,
                            format!("no base class method `{}` to call", prop.name),
                            "not found in the base class",
                        );
                        ExprKind::Error
                    }
                }
            }
            A::Member { .. } if self.names_enum_member(e) => {
                let Some((enum_, variant)) = self.variant_ref(e) else {
                    return self.error_expr(span);
                };
                ExprKind::Variant {
                    enum_,
                    variant,
                    args: None,
                }
            }
            A::Match { scrutinee, arms } => {
                let scrutinee = self.lower_expr(scrutinee);
                let arms = arms.iter().map(|a| self.arm(a)).collect();
                ExprKind::Match {
                    scrutinee: Box::new(scrutinee),
                    arms,
                    switch: None,
                }
            }
            A::Switch { scrutinee, cases } => {
                let used = !std::mem::take(&mut self.stmt_switch);
                let scrutinee = self.lower_expr(scrutinee);
                let arms = cases
                    .iter()
                    .map(|c| {
                        self.f.scopes.push(Scope::default());
                        let mut bound = Binds::default();
                        let pat = match &c.pat {
                            Some(p) => self.lower_pattern(p, &mut bound),
                            None => self.pattern(PatKind::Wild, c.span),
                        };
                        let guard = c.guard.as_ref().map(|g| self.lower_expr(g));
                        let block = self.block(&c.body);
                        let body = self.expr(ExprKind::Block(block), c.body.span);
                        self.f.scopes.pop();
                        Arm {
                            pat,
                            guard,
                            body,
                            span: c.span,
                        }
                    })
                    .collect();
                ExprKind::Match {
                    scrutinee: Box::new(scrutinee),
                    arms,
                    switch: Some(used),
                }
            }
            A::Is { expr, pat } => {
                let expr = self.lower_expr(expr);
                // In an `if`/`while` condition (through `&&`), the bindings are visible in
                // the body; elsewhere they're only defined inside the test.
                if !cond {
                    self.f.scopes.push(Scope::default());
                }
                let mut bound = Binds::default();
                let pat = self.lower_pattern(pat, &mut bound);
                if !cond {
                    self.f.scopes.pop();
                }
                ExprKind::Is {
                    expr: Box::new(expr),
                    pat,
                }
            }
            A::Call {
                callee,
                type_args,
                args,
            } => {
                let type_args = type_args.iter().map(|t| self.resolve_type(t)).collect();
                let callee = self.lower_expr(callee);
                let out = self.call_args(args);
                ExprKind::Call {
                    callee: Box::new(callee),
                    args: out,
                    type_args,
                }
            }
            A::NonNull(inner) => ExprKind::NonNull(Box::new(self.lower_expr(inner))),
            A::Member { object, prop, .. } => ExprKind::Member {
                object: Box::new(self.lower_expr(object)),
                name: prop.name.clone(),
                name_span: prop.span,
            },
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
            A::If {
                cond: c,
                then,
                else_,
            } => {
                // `is` bindings in the condition are visible in `then`.
                self.f.scopes.push(Scope::default());
                self.cond_scope = true;
                let c = self.lower_expr(c);
                self.cond_scope = false;
                let then = self.lower_expr(then);
                self.f.scopes.pop();
                ExprKind::If {
                    cond: Box::new(c),
                    then: Box::new(then),
                    else_: else_.as_ref().map(|e| Box::new(self.lower_expr(e))),
                }
            }
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
            A::Range { start, end } => ExprKind::Range {
                start: Box::new(self.lower_expr(start)),
                end: Box::new(self.lower_expr(end)),
            },
            A::Array(elems) => {
                let mut out = Vec::new();
                for el in elems {
                    match el {
                        ast::ArrayElem::Expr(x) => out.push(self.lower_expr(x)),
                        ast::ArrayElem::Spread(x) => {
                            self.not_implemented(x.span, "spread in array literals")
                        }
                    }
                }
                ExprKind::Array(out)
            }
            A::ArrayRepeat { value, count } => {
                let value = self.lower_expr(value);
                match self.const_len(count) {
                    Some(count) => ExprKind::Repeat {
                        value: Box::new(value),
                        count,
                    },
                    None => ExprKind::Error,
                }
            }
            A::Error => ExprKind::Error,
            other => {
                let what = match other {
                    A::Super => "`super` outside a method call",
                    A::Object { .. } => "object and struct literals",
                    A::Await(_) => "`await`",
                    A::Yield { .. } => "generators",
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
    /// Call arguments, with `name: value` and `...value` kept as such.
    fn call_args(&mut self, args: &[ast::Arg]) -> Vec<Expr> {
        args.iter()
            .map(|a| {
                let v = self.lower_expr(&a.value);
                let span = a.value.span;
                if a.spread {
                    return self.expr(ExprKind::Spread(Box::new(v)), span);
                }
                match &a.name {
                    Some(n) => self.expr(
                        ExprKind::NamedArg {
                            name: n.name.clone(),
                            name_span: n.span,
                            value: Box::new(v),
                        },
                        n.span.to(span),
                    ),
                    None => v,
                }
            })
            .collect()
    }

    /// `@derive(Eq, Hash, Ord)` (the only decorator so far): the protocols it names.
    fn derive_decorators(&mut self, decs: &[ast::Decorator]) -> Vec<String> {
        let mut out = Vec::new();
        for dec in decs {
            let is_derive = dec.path.len() == 1 && dec.path[0].name == "derive";
            if !is_derive {
                self.not_implemented(dec.span, "decorators other than `@derive`");
                continue;
            }
            for a in dec.args.iter().flatten() {
                match &a.value.kind {
                    ast::ExprKind::Ident(n) if matches!(n.as_str(), "Eq" | "Hash" | "Ord") => {
                        out.push(n.clone())
                    }
                    _ => self.error(
                        codes::UNKNOWN_NAME,
                        a.value.span,
                        "`@derive` takes `Eq`, `Hash` and `Ord`",
                        "not a derivable protocol",
                    ),
                }
            }
        }
        out
    }

    /// Whether `name` refers to a variable (which shadows a type of the same name).
    fn is_value_name(&self, name: &str) -> bool {
        self.f.scopes.iter().any(|s| s.names.contains_key(name))
            || self.top_globals.contains_key(name)
    }

    /// Whether `e` names a class (as in `Animal.count`), not shadowed by a variable.
    fn names_class(&self, e: &ast::Expr) -> bool {
        matches!(&e.kind, ast::ExprKind::Ident(n)
            if !self.is_value_name(n)
                && self.top_structs.get(n).is_some_and(|id| self.structs[id.0 as usize].is_class))
    }

    /// `Class.name`: a static field or static method (checking visibility).
    fn static_member(&mut self, class: &ast::Expr, prop: &ast::Ident) -> Option<ExprKind> {
        let ast::ExprKind::Ident(cname) = &class.kind else {
            return None;
        };
        let sid = self.top_structs[cname];
        let def = &self.structs[sid.0 as usize];
        let found = match (def.static_field(&prop.name), def.static_method(&prop.name)) {
            (Some(g), _) => ExprKind::Var(Var::Global(g)),
            (_, Some(f)) => ExprKind::Fn(f),
            _ => {
                let what = if def.field(&prop.name).is_some() || def.method(&prop.name).is_some() {
                    "is an instance member; use it on an object"
                } else {
                    "isn't declared"
                };
                self.error(
                    codes::UNKNOWN_MEMBER,
                    prop.span,
                    format!("class `{cname}` has no static member `{}`", prop.name),
                    format!("`{}` {what}", prop.name),
                );
                return None;
            }
        };
        if !def.is_public(&prop.name) && self.f.owner != Some(sid) {
            self.private_access(cname, &prop.name, prop.span, def.name_span);
        }
        Some(found)
    }

    fn private_access(&mut self, class: &str, member: &str, span: Span, decl: Span) {
        self.diags.push(
            Diagnostic::error(
                codes::PRIVATE_MEMBER,
                format!("`{member}` is private to `{class}`"),
            )
            .primary(span, "used outside the class")
            .secondary(decl, format!("`{class}` is declared here"))
            .help(format!(
                "class members are private by default; declare it `public {member}` to use it here"
            )),
        );
    }

    /// Whether `e` is `Enum.name` (with `Enum` not shadowed by a variable).
    fn names_enum_member(&self, e: &ast::Expr) -> bool {
        match &e.kind {
            ast::ExprKind::Member {
                object,
                optional: false,
                ..
            } => matches!(&object.kind, ast::ExprKind::Ident(n)
                if self.top_enums.contains_key(n) && !self.is_value_name(n)),
            _ => false,
        }
    }

    /// `Shape.Circle` as an expression: the enum and variant. Reports an unknown variant.
    fn variant_ref(&mut self, e: &ast::Expr) -> Option<(EnumId, u32)> {
        let ast::ExprKind::Member {
            object,
            prop,
            optional: false,
        } = &e.kind
        else {
            return None;
        };
        let ast::ExprKind::Ident(name) = &object.kind else {
            return None;
        };
        if self.is_value_name(name) {
            return None;
        }
        let eid = *self.top_enums.get(name)?;
        match self.enums[eid.0 as usize].variant(&prop.name) {
            Some((i, _)) => Some((eid, i)),
            None => {
                let def = &self.enums[eid.0 as usize];
                let names: Vec<&str> = def.variants.iter().map(|v| v.name.as_str()).collect();
                let d = Diagnostic::error(
                    codes::UNKNOWN_MEMBER,
                    format!("enum `{name}` has no variant `{}`", prop.name),
                )
                .primary(prop.span, "unknown variant")
                .secondary(def.name_span, format!("`{name}` is declared here"))
                .note(format!("its variants are: {}", names.join(", ")));
                self.diags.push(d);
                None
            }
        }
    }

    fn arm(&mut self, a: &ast::MatchArm) -> Arm {
        self.f.scopes.push(Scope::default());
        let mut bound = Binds::default();
        let pat = self.lower_pattern(&a.pat, &mut bound);
        let guard = a.guard.as_ref().map(|g| self.lower_expr(g));
        let body = self.lower_expr(&a.body);
        self.f.scopes.pop();
        Arm {
            pat,
            guard,
            body,
            span: a.span,
        }
    }

    fn pattern(&mut self, kind: PatKind, span: Span) -> Pattern {
        let id = ExprId(self.spans.len() as u32);
        self.spans.push(span);
        Pattern { id, kind, span }
    }

    /// `Shape.Circle` or `Circle` in a pattern: the enum (if named) and the variant name.
    fn variant_path(&mut self, path: &[ast::Ident]) -> Option<(Option<EnumId>, ast::Ident)> {
        match path {
            [v] => Some((None, v.clone())),
            [e, v] => match self.top_enums.get(&e.name) {
                Some(id) => Some((Some(*id), v.clone())),
                None => {
                    self.error(
                        codes::UNKNOWN_TYPE,
                        e.span,
                        format!("cannot find enum `{}`", e.name),
                        "unknown enum",
                    );
                    None
                }
            },
            _ => {
                let span = path[0].span.to(path[path.len() - 1].span);
                self.not_implemented(span, "module paths in patterns");
                None
            }
        }
    }

    /// Declares a pattern binding (or, in a later or-alternative, reuses the first one's).
    fn bind_name(&mut self, n: &ast::Ident, b: &mut Binds) -> Option<LocalId> {
        if let Some((prev, _)) = b.names.get(&n.name) {
            self.diags.push(
                Diagnostic::error(
                    codes::DUPLICATE_DECL,
                    format!("`{}` is bound twice in this pattern", n.name),
                )
                .primary(n.span, "bound again here")
                .secondary(*prev, "first bound here"),
            );
            return None;
        }
        let id = match b.reuse.as_ref().and_then(|r| r.get(&n.name)) {
            Some(id) => *id,
            None => {
                if b.reuse.is_some() {
                    self.diags.push(
                        Diagnostic::error(
                            codes::UNKNOWN_NAME,
                            format!("`{}` isn't bound in the first alternative", n.name),
                        )
                        .primary(n.span, "only bound here")
                        .note("every alternative of an or-pattern must bind the same names"),
                    );
                }
                self.declare_local(n, false, None)
            }
        };
        b.names.insert(n.name.clone(), (n.span, id));
        Some(id)
    }

    fn bind_pat(&mut self, n: &ast::Ident, b: &mut Binds) -> PatKind {
        match self.bind_name(n, b) {
            Some(id) => PatKind::Bind(id),
            None => PatKind::Wild,
        }
    }

    /// A type named in a pattern (`x is string`, `p is Point`), if `name` is one.
    fn pattern_type(&self, name: &str) -> Option<Ty> {
        if let Some(t) = Ty::from_name(name) {
            return Some(t);
        }
        if let Some(id) = self.top_structs.get(name) {
            return Some(self.struct_ty(*id));
        }
        self.top_enums.get(name).map(|id| self.enum_ty(*id))
    }

    fn field_patterns(&mut self, fields: &[ast::FieldPat], b: &mut Binds) -> Vec<FieldPat> {
        fields
            .iter()
            .map(|f| {
                if let Some(d) = &f.default {
                    self.not_implemented(d.span, "defaults in patterns");
                }
                let pat = match &f.pat {
                    Some(sub) => self.lower_pattern(sub, b),
                    None => {
                        let k = self.bind_pat(&f.name, b);
                        self.pattern(k, f.name.span)
                    }
                };
                FieldPat {
                    name: f.name.name.clone(),
                    name_span: f.name.span,
                    pat,
                }
            })
            .collect()
    }

    /// Lowers a pattern, declaring its bindings in the current scope.
    fn lower_pattern(&mut self, p: &ast::Pattern, b: &mut Binds) -> Pattern {
        use ast::PatternKind as P;
        let span = p.span;
        let kind = match &p.kind {
            P::Wildcard => PatKind::Wild,
            P::Ident(n) if self.variant_names.contains(&n.name) => PatKind::Variant {
                enum_: None,
                name: n.name.clone(),
                name_span: n.span,
                args: VariantPatArgs::None,
            },
            P::Ident(n) if !self.is_value_name(&n.name) && self.pattern_type(&n.name).is_some() => {
                PatKind::Type {
                    ty: self.pattern_type(&n.name).expect("checked"),
                    bind: None,
                    fields: None,
                }
            }
            P::Ident(n) => self.bind_pat(n, b),
            P::Path(path) => match self.variant_path(path) {
                Some((enum_, v)) => PatKind::Variant {
                    enum_,
                    name: v.name,
                    name_span: v.span,
                    args: VariantPatArgs::None,
                },
                None => PatKind::Wild,
            },
            P::Lit(_) | P::Neg(_) => match self.literal_pattern(p) {
                Some(l) => PatKind::Lit(l),
                None => PatKind::Wild,
            },
            P::Range { start, end } => {
                match (self.literal_pattern(start), self.literal_pattern(end)) {
                    (Some(a), Some(z)) => PatKind::Range(a, z),
                    _ => PatKind::Wild,
                }
            }
            P::TupleStruct { path, elems } => match self.variant_path(path) {
                Some((enum_, v)) => {
                    let elems = elems.iter().map(|e| self.lower_pattern(e, b)).collect();
                    PatKind::Variant {
                        enum_,
                        name: v.name,
                        name_span: v.span,
                        args: VariantPatArgs::Positional(elems),
                    }
                }
                None => PatKind::Wild,
            },
            P::Struct {
                path: Some(path),
                fields,
                ..
            } => {
                let fields = self.field_patterns(fields, b);
                match path.as_slice() {
                    [n] if self.top_structs.contains_key(&n.name) => PatKind::Struct {
                        sid: self.top_structs[&n.name],
                        fields,
                    },
                    _ => match self.variant_path(path) {
                        Some((enum_, v)) => PatKind::Variant {
                            enum_,
                            name: v.name,
                            name_span: v.span,
                            args: VariantPatArgs::Named(fields),
                        },
                        None => PatKind::Wild,
                    },
                }
            }
            P::Or(alts) => {
                let mut out = Vec::new();
                let mut first: Option<HashMap<String, (Span, LocalId)>> = None;
                for alt in alts {
                    let mut ab = Binds {
                        names: HashMap::new(),
                        reuse: first
                            .as_ref()
                            .map(|f| f.iter().map(|(k, (_, id))| (k.clone(), *id)).collect())
                            .or_else(|| b.reuse.clone()),
                    };
                    out.push(self.lower_pattern(alt, &mut ab));
                    match &first {
                        None => first = Some(ab.names),
                        Some(f) => {
                            for (name, (sp, _)) in f {
                                if !ab.names.contains_key(name) {
                                    self.diags.push(
                                        Diagnostic::error(
                                            codes::UNKNOWN_NAME,
                                            format!("`{name}` isn't bound in every alternative"),
                                        )
                                        .primary(alt.span, format!("doesn't bind `{name}`"))
                                        .secondary(*sp, "bound here"),
                                    );
                                }
                            }
                        }
                    }
                }
                for (name, v) in first.unwrap_or_default() {
                    b.names.entry(name).or_insert(v);
                }
                PatKind::Or(out)
            }
            P::Array(elems) => {
                let mut prefix = Vec::new();
                let mut suffix = Vec::new();
                let mut rest = None;
                for e in elems {
                    match &e.kind {
                        P::Rest(name) if rest.is_none() => {
                            rest = Some(name.as_ref().and_then(|n| self.bind_name(n, b)));
                        }
                        P::Rest(_) => {
                            self.error(
                                codes::INVALID_ASSIGN_TARGET,
                                e.span,
                                "only one `...rest` per array pattern",
                                "second rest pattern",
                            );
                        }
                        _ => {
                            let sub = self.lower_pattern(e, b);
                            if rest.is_some() {
                                suffix.push(sub);
                            } else {
                                prefix.push(sub);
                            }
                        }
                    }
                }
                PatKind::Slice {
                    prefix,
                    rest,
                    suffix,
                }
            }
            P::As { name, pat } => {
                let id = self.bind_name(name, b);
                let inner = self.lower_pattern(pat, b);
                match id {
                    Some(id) => PatKind::As(id, Box::new(inner)),
                    None => return inner,
                }
            }
            P::Typed { name, ty, fields } => {
                let ty = self.resolve_type(ty);
                let bind = self.bind_name(name, b);
                let fields = fields.as_ref().map(|f| self.field_patterns(f, b));
                PatKind::Type { ty, bind, fields }
            }
            _ => {
                self.not_implemented(span, "this kind of pattern");
                PatKind::Wild
            }
        };
        self.pattern(kind, span)
    }

    /// A literal pattern (`1`, `-1`, `"a"`, `true`, `null`).
    fn literal_pattern(&mut self, p: &ast::Pattern) -> Option<LitPat> {
        match &p.kind {
            ast::PatternKind::Lit(l) => self.lit_pattern(l, false, p.span),
            ast::PatternKind::Neg(inner) => match &inner.kind {
                ast::PatternKind::Lit(l) => self.lit_pattern(l, true, p.span),
                _ => {
                    self.not_implemented(p.span, "this kind of pattern");
                    None
                }
            },
            _ => {
                self.not_implemented(p.span, "range bounds other than literals");
                None
            }
        }
    }

    fn lit_pattern(&mut self, l: &ast::Lit, neg: bool, span: Span) -> Option<LitPat> {
        let lit = match l {
            ast::Lit::Int { value, .. } if neg => LitPat::Int(-(*value as i128)),
            ast::Lit::Int { value, .. } => LitPat::Int(*value as i128),
            ast::Lit::Float { value, .. } if neg => LitPat::Float(-value),
            ast::Lit::Float { value, .. } => LitPat::Float(*value),
            ast::Lit::Str(s) if !neg => LitPat::Str(s.clone()),
            ast::Lit::Bool(b) if !neg => LitPat::Bool(*b),
            ast::Lit::Null if !neg => LitPat::Null,
            _ => {
                self.not_implemented(span, "this kind of literal pattern");
                return None;
            }
        };
        Some(lit)
    }

    /// `a?.b.c()` → `OptChain { tmp = a, rest: tmp.b.c() }`; the base is lowered recursively,
    /// so `a?.b?.c` nests.
    fn opt_chain(&mut self, e: &ast::Expr, base: &ast::Expr) -> Expr {
        let base = self.lower_expr(base);
        let name = format!("<opt{}>", self.f.locals.len());
        let tmp = self.hidden_local(&name, base.span);
        self.f.scopes.push(Scope::default());
        self.f
            .scopes
            .last_mut()
            .expect("scope")
            .names
            .insert(name.clone(), tmp);
        let rest = rewrite_opt_link(e, &name);
        let rest = self.lower_expr(&rest);
        self.f.scopes.pop();
        self.expr(
            ExprKind::OptChain {
                tmp,
                base: Box::new(base),
                rest: Box::new(rest),
            },
            e.span,
        )
    }

    /// An assignment target: a variable, an implicit member, or `object.field`.
    fn lower_target(&mut self, e: &ast::Expr) -> Option<Target> {
        let t = strip_parens(e);
        match &t.kind {
            ast::ExprKind::Ident(name) => self.resolve_target(name, t.span),
            ast::ExprKind::Member {
                object,
                prop,
                optional: false,
            } if self.names_class(object) => match self.static_member(object, prop)? {
                ExprKind::Var(v) => {
                    self.check_mutable(v, t.span);
                    Some(Target::Var(v))
                }
                _ => {
                    self.error(
                        codes::NOT_A_VALUE,
                        t.span,
                        format!("can't assign to the static method `{}`", prop.name),
                        "not a field",
                    );
                    None
                }
            },
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
            ast::ExprKind::Index { object, index } => Some(Target::Index {
                object: Box::new(self.lower_expr(object)),
                index: Box::new(self.lower_expr(index)),
            }),
            _ => {
                self.not_implemented(
                    e.span,
                    "assignment to anything but a variable, a field or an element",
                );
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
        let mut type_args = Vec::new();
        let sid = match ty.map(|t| &t.kind) {
            None => None,
            Some(ast::TypeKind::Path { path, args }) if path.len() == 1 => {
                type_args = args.iter().map(|a| self.resolve_type(a)).collect();
                match self.top_structs.get(&path[0].name) {
                    Some(id) if self.structs[id.0 as usize].is_class => {
                        self.error(
                            codes::UNKNOWN_OBJECT_TYPE,
                            path[0].span,
                            format!("`{}` is a class", path[0].name),
                            format!("create it with `new {}(…)`", path[0].name),
                        );
                        return ExprKind::Error;
                    }
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
            type_args,
        }
    }
}

/// The object of the outermost `?.` in the member/call/index chain `e`, if there is one.
fn opt_link_base(e: &ast::Expr) -> Option<&ast::Expr> {
    let mut cur = e;
    loop {
        cur = match &cur.kind {
            ast::ExprKind::Member {
                object,
                optional: true,
                ..
            } => return Some(object),
            ast::ExprKind::Member { object, .. } | ast::ExprKind::Index { object, .. } => object,
            ast::ExprKind::Call { callee, .. } => callee,
            _ => return None,
        };
    }
}

/// `e` with its outermost `?.` link turned into `.` on the variable `name`.
fn rewrite_opt_link(e: &ast::Expr, name: &str) -> ast::Expr {
    use ast::ExprKind as A;
    let kind = match &e.kind {
        A::Member {
            object,
            prop,
            optional: true,
        } => A::Member {
            object: Box::new(ast::Expr {
                kind: A::Ident(name.to_string()),
                span: object.span,
            }),
            prop: prop.clone(),
            optional: false,
        },
        A::Member {
            object,
            prop,
            optional,
        } => A::Member {
            object: Box::new(rewrite_opt_link(object, name)),
            prop: prop.clone(),
            optional: *optional,
        },
        A::Index { object, index } => A::Index {
            object: Box::new(rewrite_opt_link(object, name)),
            index: index.clone(),
        },
        A::Call {
            callee,
            type_args,
            args,
        } => A::Call {
            callee: Box::new(rewrite_opt_link(callee, name)),
            type_args: type_args.clone(),
            args: args.clone(),
        },
        _ => unreachable!("not a chain"),
    };
    ast::Expr { kind, span: e.span }
}

/// What pass 1b learns about a class, for building its functions in pass 2.
struct ClassInfo<'a> {
    sid: StructId,
    decl: &'a ast::ClassDecl,
    /// Constructors (`None`: the synthesized default one).
    ctors: Vec<(FnId, Option<&'a ast::FnDecl>)>,
    static_init: Option<FnId>,
    /// Methods, accessors and static methods (`None` as `this`), and whether abstract.
    methods: Vec<(FnId, &'a ast::FnDecl, Option<StructId>, bool)>,
    field_inits: Vec<(&'a ast::Ident, Option<&'a ast::Expr>)>,
    static_inits: Vec<(GlobalId, &'a ast::Ident, Option<&'a ast::Expr>)>,
    static_blocks: Vec<&'a ast::Block>,
}

/// An `operator` member as a method: `op+`, `op-u` (unary), `op[]`, `op[]=`. The declaration
/// gets that name (leaked: declarations live for the whole compilation).
fn operator_method(op: &str, func: &ast::FnDecl, span: Span) -> (String, &'static ast::FnDecl) {
    let unary = func.params.is_empty() && matches!(op, "-" | "!" | "~" | "+");
    let name = if unary {
        format!("op{op}u")
    } else {
        format!("op{op}")
    };
    let mut decl = func.clone();
    decl.name = Some(ast::Ident {
        name: name.clone(),
        span,
    });
    (name, Box::leak(Box::new(decl)))
}

/// A literal type's base type and value.
fn lit_val(l: &ast::Lit) -> Option<(Ty, LitVal)> {
    Some(match l {
        ast::Lit::Str(s) => (Ty::Str, LitVal::Str(s.as_str().into())),
        ast::Lit::Int {
            value,
            suffix: None,
        } => (Ty::INT, LitVal::Int(*value as i128)),
        ast::Lit::Bool(b) => (Ty::Bool, LitVal::Bool(*b)),
        _ => return None,
    })
}

/// `super(args)` / `this(args)` as a statement: (is super, args, span).
fn ctor_call_kind(s: &ast::Stmt) -> Option<(bool, &[ast::Arg], Span)> {
    let ast::StmtKind::Expr { expr, .. } = &s.kind else {
        return None;
    };
    let ast::ExprKind::Call { callee, args, .. } = &expr.kind else {
        return None;
    };
    match callee.kind {
        ast::ExprKind::Super => Some((true, args, expr.span)),
        ast::ExprKind::This => Some((false, args, expr.span)),
        _ => None,
    }
}

/// The type of an unannotated field from its literal initializer.
fn literal_type(e: &ast::Expr) -> Option<Ty> {
    match &e.kind {
        ast::ExprKind::Lit(ast::Lit::Int { suffix: None, .. }) => Some(Ty::INT),
        ast::ExprKind::Lit(ast::Lit::Float { suffix: None, .. }) => Some(Ty::NUMBER),
        ast::ExprKind::Lit(ast::Lit::Str(_)) => Some(Ty::Str),
        ast::ExprKind::Lit(ast::Lit::Bool(_)) => Some(Ty::Bool),
        ast::ExprKind::Unary {
            op: ast::UnOp::Neg,
            expr,
        } => literal_type(expr),
        _ => None,
    }
}

/// The names a pattern binds so far; in a later or-alternative, the first one's locals.
#[derive(Default)]
struct Binds {
    names: HashMap<String, (Span, LocalId)>,
    reuse: Option<HashMap<String, LocalId>>,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Inline {
    Struct(StructId),
    Enum(EnumId),
}

/// Structs and enums stored inline in a value of type `t` (directly, or inside tuples, fixed
/// arrays and `T?`).
fn inline_types(t: &Ty, out: &mut Vec<Inline>) {
    match t {
        Ty::Struct(id, _, _) => out.push(Inline::Struct(*id)),
        Ty::Enum(id, _) => out.push(Inline::Enum(*id)),
        Ty::Fixed(e, _) | Ty::Nullable(e) => inline_types(e, out),
        Ty::Tuple(ts) => ts.iter().for_each(|t| inline_types(t, out)),
        _ => {}
    }
}

/// The names a destructuring pattern binds, in order.
fn pattern_idents<'a>(p: &'a ast::Pattern, out: &mut Vec<&'a ast::Ident>) {
    match &p.kind {
        ast::PatternKind::Ident(n) => out.push(n),
        ast::PatternKind::Array(elems) => {
            for e in elems {
                pattern_idents(e, out);
            }
        }
        _ => {}
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
