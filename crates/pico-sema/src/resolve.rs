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
use pico_diag::{Code, Diagnostic, FileId, Span, codes};
use pico_syntax::ast;
use std::collections::{HashMap, HashSet};

/// How the files of a program form modules (spec §8.1). The statements of every file come
/// in one `ast::Module`; each top-level declaration belongs to the module of its file.
#[derive(Default, Clone)]
pub struct Modules {
    /// The core library's files: their declarations are visible in every module.
    pub prelude: HashSet<FileId>,
    /// The entry module (its top level may use `let`).
    pub entry: Option<FileId>,
    pub imports: Vec<Import>,
    /// Per module: exported name → its local name.
    pub exports: HashMap<FileId, HashMap<String, String>>,
    /// How messages name each module (`./geometry`).
    pub names: HashMap<FileId, String>,
    /// `new Worker("spec")` in a module: (that module, the spec, the worker module).
    pub workers: Vec<(FileId, String, FileId)>,
    /// Decorators of top-level bindings (`@comptimeLimit(n) comptime const …`), by the span
    /// of the binding statement.
    pub binding_decorators: HashMap<Span, Vec<ast::Decorator>>,
    /// The decorators that were on structs and classes before they ran (for reflection), by
    /// the type's name span: each one's name and its arguments' source text.
    pub type_decorators: HashMap<Span, Vec<(String, Vec<String>)>>,
    /// Code generated into a module (by decorators) lives in a file of its own, part of the
    /// module of the file it maps to.
    pub file_modules: HashMap<FileId, FileId>,
}

/// `import { name as alias } from "…"` or `import * as alias from "…"`, in `module`.
#[derive(Clone)]
pub struct Import {
    pub module: FileId,
    pub target: FileId,
    /// `None`: `* as alias`.
    pub name: Option<Ident>,
    pub alias: Ident,
}

pub use pico_syntax::ast::Ident;

const PRELUDE_MODULE: u32 = u32::MAX;

/// The module being resolved and what each module sees from the others.
#[derive(Default)]
struct ModCtx {
    cur: u32,
    entry: Option<u32>,
    /// Per module: local name → (module, exported name).
    imports: HashMap<u32, HashMap<String, (u32, String)>>,
    /// Per module: `* as` alias → module.
    namespaces: HashMap<u32, HashMap<String, u32>>,
    exports: HashMap<u32, HashMap<String, String>>,
    names: HashMap<u32, String>,
}

impl ModCtx {
    /// The key `name` refers to in the current module: its own declaration, an import (followed
    /// through re-exports), or the core library's.
    fn resolve(
        &self,
        name: &str,
        exists: &dyn Fn(&(u32, String)) -> bool,
    ) -> Option<(u32, String)> {
        self.resolve_in(self.cur, name, exists, 0).or_else(|| {
            let k = (PRELUDE_MODULE, name.to_string());
            exists(&k).then_some(k)
        })
    }

    fn resolve_in(
        &self,
        m: u32,
        name: &str,
        exists: &dyn Fn(&(u32, String)) -> bool,
        depth: u32,
    ) -> Option<(u32, String)> {
        let k = (m, name.to_string());
        if exists(&k) {
            return Some(k);
        }
        if depth > 16 {
            return None;
        }
        let (tm, exported) = self.imports.get(&m)?.get(name)?;
        let local = self
            .exports
            .get(tm)
            .and_then(|e| e.get(exported))
            .cloned()
            .unwrap_or_else(|| exported.clone());
        // (A module may export a core library name: `std/meta` exports `__ClassDecl` as
        // `ClassDecl`.)
        self.resolve_in(*tm, &local, exists, depth + 1).or_else(|| {
            let k = (PRELUDE_MODULE, local);
            exists(&k).then_some(k)
        })
    }
}

/// Top-level names, by module.
struct Names<T> {
    map: HashMap<(u32, String), T>,
}

impl<T> Default for Names<T> {
    fn default() -> Self {
        Names {
            map: HashMap::new(),
        }
    }
}

impl<T> Names<T> {
    fn get(&self, m: &ModCtx, name: &str) -> Option<&T> {
        let k = m.resolve(name, &|k| self.map.contains_key(k))?;
        self.map.get(&k)
    }

    fn contains_key(&self, m: &ModCtx, name: &str) -> bool {
        self.get(m, name).is_some()
    }

    /// Whether `name` is visible and declared outside the core library.
    fn outside_prelude(&self, m: &ModCtx, name: &str) -> bool {
        m.resolve(name, &|k| self.map.contains_key(k))
            .is_some_and(|(km, _)| km != PRELUDE_MODULE)
    }

    /// Declares `name` in the current module.
    fn insert(&mut self, m: &ModCtx, name: String, v: T) {
        self.map.insert((m.cur, name), v);
    }

    /// The current module's own declaration of `name`.
    fn own_mut(&mut self, m: &ModCtx, name: &str) -> Option<&mut T> {
        self.map.get_mut(&(m.cur, name.to_string()))
    }

    /// Names visible in the current module (for suggestions).
    fn visible<'a>(&'a self, m: &'a ModCtx) -> impl Iterator<Item = &'a str> + 'a {
        self.map
            .keys()
            .filter(move |(km, _)| *km == m.cur || *km == PRELUDE_MODULE)
            .map(|(_, n)| n.as_str())
    }

    fn values(&self) -> impl Iterator<Item = &T> {
        self.map.values()
    }

    /// The core library's names.
    fn prelude(&self) -> impl Iterator<Item = (&String, &T)> {
        self.map
            .iter()
            .filter(|((m, _), _)| *m == PRELUDE_MODULE)
            .map(|((_, n), v)| (n, v))
    }
}

pub fn resolve(module: &ast::Module, file: FileId) -> (Program, Vec<Diagnostic>) {
    resolve_modules(module, file, &Modules::default())
}

pub fn resolve_modules(
    module: &ast::Module,
    file: FileId,
    modules: &Modules,
) -> (Program, Vec<Diagnostic>) {
    let mut r = Resolver {
        async_methods: HashSet::new(),
        awaits: 0,
        worker_files: Vec::new(),
        worker_ids: HashMap::new(),
        uses_inline_workers: false,
        exports: Vec::new(),
        externs: Vec::new(),
        extern_cfns: HashMap::new(),
        links: Vec::new(),
        mods: ModCtx::default(),
        prelude_files: modules.prelude.clone(),
        std_files: modules
            .names
            .iter()
            .filter(|(_, n)| n.starts_with("std/"))
            .map(|(f, _)| *f)
            .collect(),
        file_modules: modules.file_modules.clone(),
        file,
        diags: Vec::new(),
        fns: Vec::new(),
        globals: Vec::new(),
        spans: Vec::new(),
        top_fns: Names::default(),
        top_globals: Names::default(),
        globals_declared: HashSet::new(),
        f: FnState::default(),
        structs: Vec::new(),
        top_structs: Names::default(),
        enums: Vec::new(),
        top_enums: Names::default(),
        variant_names: HashSet::new(),
        stmt_switch: false,
        cond_scope: false,
        aliases: Names::default(),
        alias_cache: HashMap::new(),
        resolving_aliases: Vec::new(),
        unions: Vec::new(),
        cur_fn: FnId(0),
        distincts: 0,
        outer_fns: Vec::new(),
        name_refs: Vec::new(),
        binding_decorators: modules.binding_decorators.clone(),
        maybe_cells: HashSet::new(),
        type_decorators: modules.type_decorators.clone(),
        comptime_limit: None,
        comptime_depth: 0,
        len_params: HashSet::new(),
    };
    r.mods.entry = modules.entry.map(|f| r.module_of(f));
    for (from, spec, target) in &modules.workers {
        let id = match r.worker_files.iter().position(|f| f == target) {
            Some(i) => i + 1,
            None => {
                r.worker_files.push(*target);
                r.worker_files.len()
            }
        };
        r.worker_ids.insert((*from, spec.clone()), id as u32);
    }
    for (f, ex) in &modules.exports {
        let m = r.module_of(*f);
        r.mods.exports.insert(m, ex.clone());
    }
    for (f, n) in &modules.names {
        let m = r.module_of(*f);
        r.mods.names.insert(m, n.clone());
    }
    for imp in &modules.imports {
        let (m, t) = (r.module_of(imp.module), r.module_of(imp.target));
        match &imp.name {
            Some(name) => {
                let exported = r
                    .mods
                    .exports
                    .get(&t)
                    .is_some_and(|e| e.contains_key(&name.name));
                if !exported {
                    let target = r.mods.names.get(&t).cloned().unwrap_or_default();
                    r.error(
                        codes::NOT_EXPORTED,
                        name.span,
                        format!("`{}` isn't exported by `{target}`", name.name),
                        "not exported",
                    );
                    continue;
                }
                r.mods
                    .imports
                    .entry(m)
                    .or_default()
                    .insert(imp.alias.name.clone(), (t, name.name.clone()));
            }
            None => {
                r.mods
                    .namespaces
                    .entry(m)
                    .or_default()
                    .insert(imp.alias.name.clone(), t);
            }
        }
    }
    let mut program = r.program(module);
    for ex in modules.exports.values() {
        for (exported, local) in ex {
            if local.starts_with("__") && exported != local {
                program
                    .public_names
                    .entry(local.clone())
                    .or_insert_with(|| exported.clone());
            }
        }
    }
    // Imported names, for the language server's index.
    for imp in &modules.imports {
        let Some(name) = &imp.name else { continue };
        r.mods.cur = r.module_of(imp.module);
        if let Some(decl) = r.top_level_decl(&imp.alias.name, &program) {
            program.name_refs.push((name.span, decl));
            if imp.alias.span != name.span {
                program.name_refs.push((imp.alias.span, decl));
            }
        }
    }
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
    /// A generator's step function (`yield` is allowed).
    generator: bool,
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
    /// `comptime` parameters (`comptime N: i32`, `comptime n: i32`), by name: used as
    /// values, they are the instance's constants.
    const_params: Vec<(String, Ty)>,
    /// Local `comptime const`s and their thunks.
    comptime_consts: HashMap<LocalId, FnId>,
    /// A closure: may see the enclosing functions' locals (`captures` records them).
    is_closure: bool,
    /// An arrow function (sees the enclosing method's `this`).
    arrow: bool,
    captures: Vec<Capture>,
    /// An async function's step (`await` suspends, `return` resolves its promise `__p`).
    async_step: bool,
    /// An async generator's step (`yield v` hands `v` to its iterator `__it`).
    async_gen: bool,
}

struct Resolver {
    /// Declarations (by name span) of `const`s initialized by a call: they may hold a
    /// `Reactive<T>` (spec §16.2), so assigning them is left to the type checker.
    maybe_cells: HashSet<Span>,
    /// See [`Modules::binding_decorators`].
    binding_decorators: HashMap<Span, Vec<ast::Decorator>>,
    /// See [`Modules::type_decorators`].
    type_decorators: HashMap<Span, Vec<(String, Vec<String>)>>,
    /// The `@comptimeLimit` of the binding whose `comptime` initializer is being lowered.
    comptime_limit: Option<u64>,
    /// How many `comptime` expressions the code being lowered is inside.
    comptime_depth: u32,
    /// `comptime` parameters used as array lengths (owner, index).
    len_params: HashSet<(u32, u32)>,
    /// The module being resolved and the import tables.
    mods: ModCtx,
    prelude_files: HashSet<FileId>,
    std_files: HashSet<FileId>,
    /// See [`Modules::file_modules`].
    file_modules: HashMap<FileId, FileId>,
    file: FileId,
    diags: Vec<Diagnostic>,
    fns: Vec<Option<Function>>,
    globals: Vec<Binding>,
    spans: Vec<Span>,
    /// Top-level functions by name (several for overloads).
    top_fns: Names<Vec<FnId>>,
    top_globals: Names<GlobalId>,
    /// Async methods (by their declaration's span): desugared like async functions.
    async_methods: HashSet<Span>,
    /// `new Worker(f)` appears (the program needs the inline workers' entry function).
    uses_inline_workers: bool,
    /// `@export(…)` functions.
    exports: Vec<Export>,
    /// `extern "C"` functions, and which of their parameters are C function pointers.
    externs: Vec<Extern>,
    extern_cfns: HashMap<FnId, Vec<bool>>,
    /// `@link(…)` libraries.
    links: Vec<String>,
    /// Worker modules by id - 1, and what `new Worker("spec")` in a module refers to.
    worker_files: Vec<FileId>,
    worker_ids: HashMap<(FileId, String), u32>,
    /// `await`s desugared so far (their temporaries get unique names).
    awaits: u32,
    globals_declared: HashSet<GlobalId>,
    f: FnState,
    structs: Vec<StructDef>,
    top_structs: Names<StructId>,
    enums: Vec<EnumDef>,
    top_enums: Names<EnumId>,
    /// Every variant name of every enum: an identifier pattern with one of these names is a
    /// variant pattern, not a binding.
    variant_names: HashSet<String>,
    /// The `switch` being lowered is a statement (its value isn't used).
    stmt_switch: bool,
    /// Lowering an `if`/`while` condition, where `is` bindings stay in scope for the body.
    cond_scope: bool,
    /// `type Name = T` declarations, resolved on first use.
    aliases: Names<(ast::Type, Span)>,
    alias_cache: HashMap<(u32, String), Ty>,
    resolving_aliases: Vec<String>,
    /// Interned unions: the members and the anonymous enum that stores them.
    unions: Vec<(Vec<Ty>, EnumId)>,
    /// The function being resolved (owner of its type parameters).
    cur_fn: FnId,
    /// Distinct types created so far (each gets its own id).
    distincts: u32,
    /// The states of the functions enclosing the closure being resolved, outermost first.
    outer_fns: Vec<FnState>,
    /// See [`Program::name_refs`].
    name_refs: Vec<(Span, Span)>,
}

enum Found {
    Var(Var),
    Fn(FnId),
    Overloads(Vec<FnId>),
    Builtin(Builtin),
    /// A field or method of `this` (implicit member).
    Member(String),
    /// A `comptime` parameter (`Ty::Param`).
    Const(Ty),
    /// A type, used as a value.
    Type(Ty),
    /// Inside comptime code: a local `comptime const` of the function around it, by the
    /// thunk that computes it.
    Thunk(FnId),
}

impl Resolver {
    fn module_of(&self, f: FileId) -> u32 {
        let f = self.file_modules.get(&f).copied().unwrap_or(f);
        if self.prelude_files.contains(&f) {
            PRELUDE_MODULE
        } else {
            f.0
        }
    }

    /// Resolves what follows in the module of `span`'s file.
    fn enter_module(&mut self, span: Span) {
        self.mods.cur = self.module_of(span.file);
    }

    fn error(&mut self, code: Code, span: Span, msg: impl Into<String>, label: impl Into<String>) {
        self.diags
            .push(Diagnostic::error(code, msg).primary(span, label));
    }

    /// An `extern "C" { … }` block (spec §12.1): its functions are declared like top-level
    /// ones (their bodies panic in the interpreter; native code calls the C symbol), and
    /// `@link("name")` on it asks for a library.
    fn extern_block<'a>(
        &mut self,
        d: &ast::Decl,
        abi: &str,
        items: &'a [ast::Decl],
        first_seen: &mut HashMap<String, Span>,
        fn_decls: &mut Vec<(FnId, &'a ast::FnDecl, Option<StructId>)>,
    ) {
        if abi != "C" {
            self.not_implemented(d.span, "`extern` blocks for ABIs other than \"C\"");
            return;
        }
        for dec in &d.decorators {
            let lib = match (&dec.path[..], dec.args.as_deref()) {
                ([p], Some([a])) if p.name == "link" => match &a.value.kind {
                    ast::ExprKind::Lit(ast::Lit::Str(s)) => Some(s.clone()),
                    _ => None,
                },
                _ => None,
            };
            match lib {
                Some(l) => {
                    if !self.links.contains(&l) {
                        self.links.push(l)
                    }
                }
                None => self.error(
                    codes::DECORATOR,
                    dec.span,
                    "an `extern` block takes only `@link(\"library\")`",
                    "e.g. `@link(\"SDL2\")`",
                ),
            }
        }
        for item in items {
            let ast::DeclKind::Function(f) = &item.kind else {
                self.not_implemented(
                    item.span,
                    "declarations other than functions in `extern` blocks",
                );
                continue;
            };
            let Some(name) = &f.name else { continue };
            if f.body.is_some() || !f.generics.is_empty() || f.flags.is_async {
                self.error(
                    codes::NOT_IMPLEMENTED,
                    f.span,
                    "an `extern` function is a declaration: no body, type parameters or `async`",
                    "declare the C function's signature only",
                );
                continue;
            }
            if self.check_duplicate(first_seen, name) {
                continue;
            }
            let id = FnId(self.fns.len() as u32);
            self.fns.push(None);
            self.top_fns.insert(&self.mods, name.name.clone(), vec![id]);
            fn_decls.push((id, f, None));
            let cfns = f
                .params
                .iter()
                .map(|p| {
                    matches!(&p.ty, Some(t) if matches!(&t.kind, ast::TypeKind::Function { extern_abi: Some(_), .. }))
                })
                .collect();
            self.extern_cfns.insert(id, cfns);
            self.externs.push(Extern {
                func: id,
                symbol: name.name.clone(),
            });
        }
    }

    /// `@export("js")` / `@export("c")` on a top-level function (spec §12.3, §12.4): which,
    /// and the decorator's span. Other decorators on functions aren't implemented here (user
    /// decorators were expanded before resolution).
    fn export_decorator(&mut self, d: &ast::Decl, f: &ast::FnDecl) -> Option<(ExportAbi, Span)> {
        let mut found = None;
        for dec in &d.decorators {
            let is_export = dec.path.len() == 1 && dec.path[0].name == "export";
            if !is_export {
                self.not_implemented(dec.span, "decorators");
                continue;
            }
            let abi = match dec.args.as_deref() {
                Some([a]) if a.name.is_none() => match &a.value.kind {
                    ast::ExprKind::Lit(ast::Lit::Str(s)) if s == "js" => Some(ExportAbi::Js),
                    ast::ExprKind::Lit(ast::Lit::Str(s)) if s == "c" => Some(ExportAbi::C),
                    _ => None,
                },
                _ => None,
            };
            let Some(abi) = abi else {
                self.error(
                    codes::DECORATOR,
                    dec.span,
                    "`@export` takes \"js\" or \"c\"",
                    "e.g. `@export(\"js\")`",
                );
                continue;
            };
            if !d.exported {
                self.error(
                    codes::EXPORT_TYPE,
                    dec.span,
                    "an `@export`ed function must be exported from its module too",
                    "write `export function`",
                );
            }
            if !f.generics.is_empty() || f.flags.is_async || f.flags.generator {
                self.error(
                    codes::EXPORT_TYPE,
                    dec.span,
                    "generic, async and generator functions can't be `@export`ed yet",
                    "export a plain function",
                );
            }
            if found.is_some() {
                self.error(
                    codes::DECORATOR,
                    dec.span,
                    "`@export` given twice",
                    "keep one",
                );
            }
            found = Some((abi, dec.span));
        }
        found
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
            self.enter_module(stmt.span);
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
                    .insert(&self.mods, name.name.clone(), (ty.clone(), name.span));
            }
        }
        for stmt in &module.stmts {
            self.enter_module(stmt.span);
            match &stmt.kind {
                ast::StmtKind::Decl(d) => {
                    if let ast::DeclKind::Struct(sd) = &d.kind {
                        let is_pod = |dec: &ast::Decorator| {
                            dec.path.len() == 1 && dec.path[0].name == "pod" && dec.args.is_none()
                        };
                        if let Some(dec) = d.decorators.iter().find(|d| !is_pod(d)) {
                            self.not_implemented(dec.span, "decorators");
                        }
                        let pod = d.decorators.iter().any(is_pod);
                        if pod && let Some(g) = sd.generics.first() {
                            self.not_implemented(g.name.span, "generic `@pod` structs");
                        }
                        if self.check_duplicate(&mut first_seen, &sd.name) {
                            continue;
                        }
                        let id = StructId(self.structs.len() as u32);
                        let mut def = StructDef::new(sd.name.name.clone(), sd.name.span, false);
                        def.pod = pod && sd.generics.is_empty();
                        def.decorators = self
                            .type_decorators
                            .remove(&sd.name.span)
                            .unwrap_or_default();
                        if pod {
                            def.decorators.push(("pod".into(), Vec::new()));
                        }
                        self.structs.push(def);
                        self.top_structs
                            .insert(&self.mods, sd.name.name.clone(), id);
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
                        self.top_structs
                            .insert(&self.mods, id_.name.name.clone(), id);
                        iface_decls.push((id, id_));
                    }
                    if let ast::DeclKind::Class(cd) = &d.kind {
                        self.unknown_decorators(&d.decorators);
                        if self.check_duplicate(&mut first_seen, &cd.name) {
                            continue;
                        }
                        let id = StructId(self.structs.len() as u32);
                        let mut def = StructDef::new(cd.name.name.clone(), cd.name.span, true);
                        def.decorators = self
                            .type_decorators
                            .remove(&cd.name.span)
                            .unwrap_or_default();
                        self.structs.push(def);
                        self.top_structs
                            .insert(&self.mods, cd.name.name.clone(), id);
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
                            throwable: false,
                        });
                        self.top_enums.insert(&self.mods, ed.name.name.clone(), id);
                        for v in &ed.variants {
                            self.variant_names.insert(v.name.name.clone());
                        }
                        enum_decls.push((id, ed));
                    }
                    if let ast::DeclKind::Extern { abi, items } = &d.kind {
                        self.extern_block(d, abi, items, &mut first_seen, &mut fn_decls);
                    }
                    if let ast::DeclKind::Function(f) = &d.kind
                        && let Some(name) = &f.name
                    {
                        let export = self.export_decorator(d, f);
                        // Functions may share a name (overloads); anything else may not.
                        let overload = self.top_fns.own_mut(&self.mods, &name.name).is_some();
                        if !overload && self.check_duplicate(&mut first_seen, name) {
                            continue;
                        }
                        let id = FnId(self.fns.len() as u32);
                        if let Some((abi, span)) = export {
                            if overload {
                                self.error(
                                    codes::EXPORT_TYPE,
                                    span,
                                    format!(
                                        "`{}` is overloaded, so it can't be exported",
                                        name.name
                                    ),
                                    "exported functions need a name of their own",
                                );
                            }
                            self.exports.push(Export {
                                func: id,
                                abi,
                                name: name.name.clone(),
                                span,
                            });
                        }
                        self.fns.push(None);
                        match self.top_fns.own_mut(&self.mods, &name.name) {
                            Some(v) => v.push(id),
                            None => self.top_fns.insert(&self.mods, name.name.clone(), vec![id]),
                        }
                        fn_decls.push((id, f, None));
                    }
                }
                ast::StmtKind::Let { kind, pat, ty, .. } => {
                    // Only the entry module has mutable state (spec §4.2).
                    if *kind == ast::BindingKind::Let
                        && self.mods.cur != PRELUDE_MODULE
                        && self.mods.entry.is_some_and(|e| e != self.mods.cur)
                        && !self.worker_files.contains(&stmt.span.file)
                    {
                        self.error(
                            codes::LIBRARY_LET,
                            stmt.span,
                            "a library module can't have module-level `let`",
                            "use `const`",
                        );
                    }
                    // A destructuring pattern's annotation types the whole value, so its
                    // names get their types from the initializer.
                    // (A `reactive` binding's type comes from its `__reactive<T>(v)` cell.)
                    let ann = match &pat.kind {
                        ast::PatternKind::Ident(_) if *kind != ast::BindingKind::Reactive => {
                            ty.as_ref()
                        }
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
                        let call_init = matches!(&stmt.kind,
                            ast::StmtKind::Let { init: Some(i), .. }
                                if matches!(strip_parens(i).kind, ast::ExprKind::Call { .. }));
                        if *kind == ast::BindingKind::Reactive
                            || (*kind == ast::BindingKind::Const && call_init)
                        {
                            self.maybe_cells.insert(name.span);
                        }
                        self.globals.push(Binding {
                            name: name.name.clone(),
                            mutable: *kind == ast::BindingKind::Let,
                            ty: None,
                            span: name.span,
                            boxed: false,
                        });
                        self.top_globals.insert(&self.mods, name.name.clone(), id);
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
            let span = self.structs[sid.0 as usize].name_span;
            self.enter_module(span);
            let tps = self.type_params(gs, &[], Ty::type_owner(sid));
            self.f.type_params.clear();
            self.structs[sid.0 as usize].generics = tps;
        }
        for (g, t) in pending_globals {
            self.enter_module(self.globals[g.0 as usize].span);
            let ty = self.resolve_type(t);
            self.globals[g.0 as usize].ty = Some(ty);
        }

        // Pass 1b: interfaces first (implementers need their method lists), then struct
        // fields and methods (all type names are known now).
        let mut iface_methods = Vec::new();
        for (sid, id_) in self.order_interfaces(&iface_decls) {
            self.enter_module(id_.name.span);
            self.interface_members(sid, id_, &mut iface_methods);
        }
        for (sid, sd) in &struct_decls {
            self.enter_module(sd.name.span);
            self.struct_members(*sid, sd, &mut fn_decls);
            self.f.type_params.clear();
        }
        for (eid, ed) in &enum_decls {
            self.enter_module(ed.name.span);
            self.enum_variants(*eid, ed);
        }
        let mut classes = Vec::new();
        for (sid, cd) in self.order_classes(&class_decls) {
            self.enter_module(cd.name.span);
            classes.push(self.class_members(sid, cd));
            self.f.type_params.clear();
        }
        self.check_struct_cycles();
        self.check_pod();
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
            is_unsafe: false,
            this: None,
            owner: None,
            generics: Vec::new(),
            closure: None,
            defaults: Vec::new(),
            rest: false,
            c_variadic: false,
            throws: Vec::new(),
            infer_throws: false,
            generator: false,
        });

        let reactive_class = self
            .top_structs
            .prelude()
            .find(|(n, _)| *n == "Reactive")
            .map(|(_, v)| v)
            .copied();
        // `computed(getter)`, not `computed({ get, set })`: the overload taking a function.
        let readonly_computed = self
            .top_fns
            .prelude()
            .find(|(n, _)| *n == "computed")
            .and_then(|(_, ids)| {
                ids.iter().copied().find(|id| {
                    self.fns[id.0 as usize].as_ref().is_some_and(|f| {
                        f.params
                            .first()
                            .is_some_and(|p| matches!(f.local(*p).ty, Some(Ty::Fn(..))))
                    })
                })
            });
        Program {
            reactive_class,
            readonly_computed,
            prelude_files: self.prelude_files.clone(),
            std_files: self.std_files.clone(),
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
            error_class: self
                .top_structs
                .prelude()
                .find(|(n, _)| *n == "Error")
                .map(|(_, v)| v)
                .copied(),
            iterator_class: self
                .top_structs
                .prelude()
                .find(|(n, _)| *n == "Iterator")
                .map(|(_, v)| v)
                .copied(),
            throwable_iface: self
                .top_structs
                .prelude()
                .find(|(n, _)| *n == "Throwable")
                .map(|(_, v)| *v),
            extensions: self
                .top_fns
                .prelude()
                .filter_map(|(n, ids)| {
                    let rest = n.strip_prefix("__")?;
                    let (ty, m) = rest.split_once('_')?;
                    Some(((ty.to_string(), m.to_string()), ids.clone()))
                })
                .collect(),
            static_inits,
            workers: std::mem::take(&mut self.worker_files),
            uses_inline_workers: self.uses_inline_workers,
            exports: std::mem::take(&mut self.exports),
            externs: std::mem::take(&mut self.externs),
            links: std::mem::take(&mut self.links),
            name_refs: std::mem::take(&mut self.name_refs),
            public_names: HashMap::new(),
        }
    }

    fn enum_variants(&mut self, eid: EnumId, ed: &ast::EnumDecl) {
        if let Some(g) = ed.generics.first() {
            self.not_implemented(g.span, "generic enums");
        }
        for t in &ed.implements {
            let throwable = self
                .top_structs
                .prelude()
                .find(|(n, _)| *n == "Throwable")
                .map(|(_, v)| *v);
            match self.resolve_type(t) {
                Ty::Iface(i, _, _) if Some(i) == throwable => {
                    self.enums[eid.0 as usize].throwable = true;
                }
                Ty::Error => {}
                _ => self.not_implemented(t.span, "interfaces on enums (other than `Throwable`)"),
            }
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
                    weak: false,
                    json_name: None,
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
            let json_name = self.json_name(&m.decorators);
            let is_static = m.mods.is_static
                && matches!(
                    m.kind,
                    ast::MemberKind::Method(_) | ast::MemberKind::Getter(_)
                );
            // An async method is desugared like a class's (`async_methods`).
            if m.mods.is_async
                && let ast::MemberKind::Method(f) = &m.kind
            {
                self.async_methods.insert(f.span);
            }
            if m.mods.reactive {
                self.error(
                    codes::REACTIVE,
                    m.span,
                    "a struct's fields can't be `reactive`",
                    "structs are values: use a class",
                );
            }
            let mods_ok = ast::Modifiers {
                readonly: false,
                reactive: false,
                is_static: m.mods.is_static && !is_static,
                is_async: m.mods.is_async && !matches!(m.kind, ast::MemberKind::Method(_)),
                ..m.mods
            };
            if !mods_ok.is_empty() {
                self.not_implemented(
                    m.span,
                    "member modifiers other than `readonly` and `static`",
                );
            }
            if is_static {
                // A static method or getter: a function of the type's namespace (no `this`).
                let (ast::MemberKind::Method(f) | ast::MemberKind::Getter(f)) = &m.kind else {
                    unreachable!("checked")
                };
                let Some(name) = &f.name else { continue };
                if self.check_duplicate(&mut seen, name) {
                    continue;
                }
                let id = FnId(self.fns.len() as u32);
                self.fns.push(None);
                let d = &mut self.structs[sid.0 as usize];
                if matches!(m.kind, ast::MemberKind::Getter(_)) {
                    d.static_getters.push((name.name.clone(), id));
                } else {
                    d.static_methods.push((name.name.clone(), id));
                }
                fn_decls.push((id, f, None));
                continue;
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
                    if init.is_some() {
                        self.not_implemented(m.span, "field defaults");
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
                    // `name?: T` is a `T?` that a literal may leave out (`null`).
                    let ty = if *optional { ty.nullable() } else { ty };
                    self.structs[sid.0 as usize].fields.push(FieldDef {
                        name: name.name.clone(),
                        ty,
                        span: name.span,
                        preassigned: *optional,
                        readonly,
                        weak: false,
                        json_name,
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
    /// `@pod` structs hold only plain data: numbers, `bool`, `char`, `ptr<T>`, fixed arrays of
    /// plain data and other `@pod` structs (spec §9.1).
    fn check_pod(&mut self) {
        fn plain(r: &Resolver, t: &Ty) -> bool {
            match t.erase() {
                Ty::Int(_) | Ty::Float(_) | Ty::Bool | Ty::Char | Ty::Ref(_) | Ty::Error => true,
                Ty::Fixed(e, _) => plain(r, &e),
                Ty::Struct(sid, _, _) => r.structs[sid.0 as usize].pod,
                _ => false,
            }
        }
        let mut bad = Vec::new();
        for s in self.structs.iter().filter(|s| s.pod) {
            for f in &s.fields {
                if !plain(self, &f.ty) {
                    bad.push((s.name.clone(), f.name.clone(), f.ty.clone(), f.span));
                }
            }
        }
        for (sname, fname, ty, span) in bad {
            self.diags.push(
                Diagnostic::error(
                    codes::POD_REFERENCE,
                    format!("field `{fname}` of `@pod struct {sname}` isn't plain data"),
                )
                .primary(span, format!("`{ty}`"))
                .note("`@pod` structs hold numbers, `bool`, `char`, `ptr<T>`, fixed arrays and other `@pod` structs"),
            );
        }
    }

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
    /// Whether `name` is already declared in the current module (reporting it).
    fn check_duplicate(&mut self, seen: &mut HashMap<String, Span>, name: &ast::Ident) -> bool {
        let key = format!("{}#{}", self.mods.cur, name.name);
        if let Some(&prev) = seen.get(&key) {
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
        seen.insert(key, name.span);
        false
    }

    fn top_level(&mut self, stmts: &[ast::Stmt], span: Span) -> Block {
        let mut out = Vec::new();
        let mut tail = None;
        for (i, stmt) in stmts.iter().enumerate() {
            self.enter_module(stmt.span);
            let rewritten = self
                .comptime_binding(stmt)
                .or_else(|| self.reactive_binding(stmt));
            let stmt = rewritten.as_ref().unwrap_or(stmt);
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
                    match self.top_globals.get(&self.mods, &name.name).copied() {
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
                            | ast::DeclKind::Extern { .. }
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
        self.enter_module(f.span);
        let name = f.name.clone().expect("named function");
        if (f.flags.is_async || self.async_methods.contains(&f.span))
            && f.body.is_some()
            && let Some(d) = if f.flags.generator {
                self.desugar_async_gen(f)
            } else {
                self.desugar_async(f)
            }
        {
            self.async_methods.remove(&f.span);
            return self.function_in(&d, is_entry, this_struct, owner);
        }
        if f.flags.decorator {
            self.not_implemented(name.span, "decorator functions");
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
        let mut generics = self.type_params(&f.generics, &f.where_clause, self.cur_fn.0);
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
        let (f_params, destructured) = split_destructured(&f.params);
        for (src_i, p) in f_params.iter().enumerate() {
            let ast::PatternKind::Ident(pname) = &p.pat.kind else {
                unreachable!("destructured parameters were renamed");
            };
            // `comptime T: type`: only a hidden type parameter, decided by the type written
            // as the argument (`describe(Point)`); no runtime parameter.
            if p.comptime && p.ty.as_ref().is_some_and(is_type_kind) {
                let gi = generics.len();
                let param = Ty::Param(self.cur_fn.0, gi as u32, pname.name.as_str().into());
                self.f.type_params.push((pname.name.clone(), param));
                generics.push(TypeParam {
                    name: pname.name.clone(),
                    span: pname.span,
                    bounds: Vec::new(),
                    default: None,
                    comptime: None,
                    value_param: Some(src_i),
                    is_len: false,
                });
                continue;
            }
            if !p.mods.is_empty() {
                self.not_implemented(p.span, "parameter modifiers");
            }
            // `comptime n: i32`: a hidden type parameter decided by the (constant) argument;
            // the parameter itself stays (unused) under a name nothing can refer to.
            let comptime_name = p.comptime.then(|| {
                let gi = generics.len();
                let param = Ty::Param(self.cur_fn.0, gi as u32, pname.name.as_str().into());
                self.f.const_params.push((pname.name.clone(), param));
                generics.push(TypeParam {
                    name: pname.name.clone(),
                    span: pname.span,
                    bounds: Vec::new(),
                    default: None,
                    comptime: None,
                    value_param: Some(src_i),
                    is_len: false,
                });
                ast::Ident {
                    name: format!("<comptime {}>", pname.name),
                    span: pname.span,
                }
            });
            let pname = comptime_name.as_ref().unwrap_or(pname);
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
            if p.comptime
                && let Some(g) = generics.last_mut()
            {
                g.comptime = ty.clone();
            }
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
        // `function* g(): Iterator<T> { body }` is `{ new Iterator(<step function>) }`, where
        // the step function runs `body` up to the next `yield`.
        let generated;
        let body_src = if f.flags.generator {
            let span = f.span;
            let step = ast::FnDecl {
                name: None,
                flags: ast::FnFlags {
                    arrow: true,
                    generator: true,
                    ..Default::default()
                },
                generics: Vec::new(),
                params: Vec::new(),
                ret: None,
                throws: None,
                where_clause: Vec::new(),
                body: f.body.clone(),
                span,
            };
            let new = ast::Expr {
                kind: ast::ExprKind::New {
                    callee: Box::new(ast::Expr {
                        kind: ast::ExprKind::Ident("Iterator".into()),
                        span,
                    }),
                    type_args: Vec::new(),
                    args: vec![ast::Arg {
                        name: None,
                        spread: false,
                        value: ast::Expr {
                            kind: ast::ExprKind::Function(Box::new(step)),
                            span,
                        },
                    }],
                },
                span,
            };
            generated = Some(ast::FnBody::Expr(Box::new(new)));
            &generated
        } else {
            &f.body
        };
        let pre = self.destructured_params(&destructured);
        let mut body = match body_src {
            Some(ast::FnBody::Block(b)) => self.block(b),
            Some(ast::FnBody::Expr(e)) => {
                let tail = self.lower_expr(e);
                Block {
                    stmts: Vec::new(),
                    tail: Some(Box::new(tail)),
                    span: e.span,
                }
            }
            None if self.extern_cfns.contains_key(&self.cur_fn) => {
                // The interpreter can't call C: native builds replace this body with the call.
                let msg = format!("`{}` is a C function: it needs a native build", name.name);
                let lit = |kind| ast::Expr { kind, span: f.span };
                let call = lit(ast::ExprKind::Call {
                    callee: Box::new(lit(ast::ExprKind::Ident("panic".into()))),
                    type_args: Vec::new(),
                    args: vec![ast::Arg {
                        name: None,
                        spread: false,
                        value: lit(ast::ExprKind::Lit(ast::Lit::Str(msg))),
                    }],
                });
                let tail = self.lower_expr(&call);
                Block {
                    stmts: Vec::new(),
                    tail: Some(Box::new(tail)),
                    span: f.span,
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
        body.stmts.splice(0..0, pre);
        let full_name = match this_struct {
            Some(sid) => format!("{}.{}", self.structs[sid.0 as usize].name, name.name),
            None => name.name.clone(),
        };
        for (i, g) in generics.iter_mut().enumerate() {
            g.is_len = self.len_params.contains(&(self.cur_fn.0, i as u32));
        }
        // An extern function taking or returning pointers (callbacks too) is unsafe to call
        // (spec §12.1).
        let is_extern = self.extern_cfns.contains_key(&self.cur_fn) && f.body.is_none();
        let raw = |t: &Option<Ty>| match t {
            Some(Ty::Ref(_)) => true,
            Some(Ty::Nullable(t)) => matches!(**t, Ty::Ref(_)),
            _ => false,
        };
        let unsafe_extern =
            is_extern && (raw(&ret) || params.iter().any(|p| raw(&self.f.locals[p.0 as usize].ty)));
        if f.flags.c_variadic && !is_extern {
            self.error(
                codes::EXPORT_TYPE,
                f.span,
                "only an `extern \"C\"` function can end with C's `...`",
                "use a rest parameter: `...xs: T[]`",
            );
        }
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
            is_unsafe: f.flags.is_unsafe || unsafe_extern,
            this: self.f.this,
            owner,
            generics,
            closure: None,
            defaults,
            rest,
            c_variadic: is_extern && f.flags.c_variadic,
            throws,
            infer_throws,
            generator: false,
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
                ty @ Ty::Struct(..) if self.is_throwable(&ty) => out.push(ty),
                ty @ Ty::Enum(..) if self.is_throwable(&ty) => out.push(ty),
                Ty::Never => {}
                Ty::Error => {}
                other => self.error(
                    codes::NOT_THROWABLE,
                    m.span,
                    format!("`{other}` can't be thrown"),
                    "`throws` lists `Error` classes and `Throwable` types",
                ),
            }
        }
        out
    }

    /// A struct implementing `Throwable`, or a throwable enum.
    fn is_throwable(&self, t: &Ty) -> bool {
        let iface = self
            .top_structs
            .prelude()
            .find(|(n, _)| *n == "Throwable")
            .map(|(_, v)| *v);
        match t {
            Ty::Struct(sid, _, _) => {
                iface.is_some_and(|i| self.structs[sid.0 as usize].implements.contains(&i))
            }
            Ty::Enum(e, _) => self.enums[e.0 as usize].throwable,
            _ => false,
        }
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
            let param = Ty::Param(owner, i as u32, g.name.name.as_str().into());
            let comptime = match &g.kind {
                ast::GenericKind::Type { .. } => {
                    self.f.type_params.push((g.name.name.clone(), param));
                    None
                }
                // `comptime N: i32`: a constant, used as a value or an array length.
                // `comptime T: type` is an ordinary type parameter.
                ast::GenericKind::Comptime { ty } if is_type_kind(ty) => {
                    self.f.type_params.push((g.name.name.clone(), param));
                    None
                }
                ast::GenericKind::Comptime { ty } => {
                    self.f.const_params.push((g.name.name.clone(), param));
                    Some(self.resolve_type(ty))
                }
            };
            out.push(TypeParam {
                name: g.name.name.clone(),
                span: g.name.span,
                bounds: Vec::new(),
                default: None,
                comptime,
                value_param: None,
                is_len: false,
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
            ast::TypeKind::Path { path, .. } if path.len() == 1 && path[0].name == "Hashable" => {
                vec![Bound::Hashable]
            }
            ast::TypeKind::Path { path, args, .. }
                if path.len() == 1 && path[0].name == "object" && args.is_empty() =>
            {
                vec![Bound::Object]
            }
            ast::TypeKind::Path { path, args, .. }
                if path.len() == 1 && path[0].name == "Sendable" && args.is_empty() =>
            {
                vec![Bound::Sendable]
            }
            _ => match self.resolve_type(t) {
                Ty::Iface(sid, _, _) => vec![Bound::Iface(sid)],
                // `E extends Error`: what a function throws.
                Ty::Class(sid, _, _)
                    if Some(sid)
                        == self
                            .top_structs
                            .prelude()
                            .find(|(n, _)| *n == "Error")
                            .map(|(_, v)| v)
                            .copied() =>
                {
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
            self.enter_module(cd.name.span);
            let Some(t) = &cd.extends else { continue };
            let base = match &t.kind {
                ast::TypeKind::Path { path, args } if path.len() == 1 && args.is_empty() => self
                    .top_structs
                    .get(&self.mods, &path[0].name)
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
                weak: false,
                json_name: None,
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
            reactive_fields: HashMap::new(),
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
            // `@weak` fields: the core library's weak references (spec §9.3).
            let weak = self.mods.cur == PRELUDE_MODULE
                && matches!(m.kind, ast::MemberKind::Field { .. })
                && m.decorators.len() == 1
                && m.decorators[0].args.is_none()
                && m.decorators[0].path.len() == 1
                && m.decorators[0].path[0].name == "weak";
            let json_name = if weak {
                None
            } else if matches!(m.kind, ast::MemberKind::Field { .. }) {
                self.json_name(&m.decorators)
            } else {
                if let Some(dec) = m.decorators.first() {
                    self.not_implemented(dec.span, "decorators");
                }
                None
            };
            if m.mods.is_async {
                match &m.kind {
                    ast::MemberKind::Method(f) => {
                        self.async_methods.insert(f.span);
                    }
                    _ => self.not_implemented(m.span, "async accessors and constructors"),
                }
            }
            match &m.kind {
                ast::MemberKind::Constructor(f) => {
                    // Parameter properties declare fields.
                    let mut tys = Vec::new();
                    for p in &f.params {
                        let ty = match &p.ty {
                            Some(t) if p.optional => self.resolve_type(t).nullable(),
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
                            weak: false,
                            json_name: None,
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
                    if m.mods.reactive {
                        if !self.structs[sid.0 as usize].is_class {
                            self.error(
                                codes::REACTIVE,
                                name.span,
                                "a struct's fields can't be `reactive`",
                                "structs are values: use a class",
                            );
                        } else if init.is_none() {
                            self.error(
                                codes::REACTIVE,
                                name.span,
                                format!(
                                    "the reactive field `{}` needs an initial value",
                                    name.name
                                ),
                                "add `= value`",
                            );
                        }
                        t = t.map(|t| self.reactive_ty(t));
                        info.reactive_fields.insert(name.name.clone(), ty.as_ref());
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
                            // (A reactive one holds its cell: writes go through it.)
                            mutable: !m.mods.readonly && !m.mods.reactive,
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
                    if weak && !nullable {
                        self.error(
                            codes::MISMATCHED_TYPES,
                            name.span,
                            "a `@weak` field must be nullable",
                            "the collector sets it to `null`",
                        );
                    }
                    set_vis(self, &name.name, &m.mods);
                    self.structs[sid.0 as usize].fields.push(FieldDef {
                        name: name.name.clone(),
                        ty,
                        span: name.span,
                        preassigned: init.is_some() || nullable,
                        readonly: m.mods.readonly,
                        weak,
                        json_name: json_name.clone(),
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
                        let d = &mut self.structs[sid.0 as usize];
                        match &m.kind {
                            ast::MemberKind::Getter(_) => {
                                d.static_getters.push((name.name.clone(), id));
                            }
                            ast::MemberKind::Setter(_) => {
                                self.not_implemented(m.span, "static setters");
                            }
                            _ => d.static_methods.push((name.name.clone(), id)),
                        }
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
        self.enter_module(c.decl.name.span);
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
        let mut defaults: Vec<Option<Expr>> = Vec::new();
        let mut has_rest = false;
        match decl {
            Some(f) => {
                for p in &f.params {
                    let ast::PatternKind::Ident(pname) = &p.pat.kind else {
                        self.not_implemented(p.pat.span, "destructured parameters");
                        continue;
                    };
                    if p.comptime {
                        self.not_implemented(p.span, "comptime constructor parameters");
                    }
                    // A default sees the parameters before it (as a function's, §5.3).
                    let default = match (&p.default, p.optional) {
                        (Some(d), _) => Some(self.lower_expr(d)),
                        (None, true) => Some(self.expr(ExprKind::Null, p.span)),
                        (None, false) => None,
                    };
                    has_rest |= p.rest;
                    defaults.push(default);
                    let ty = match &p.ty {
                        Some(t) => {
                            let t = self.resolve_type(t);
                            Some(if p.optional { t.nullable() } else { t })
                        }
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
                    Some(e) => match c.reactive_fields.get(&name.name) {
                        Some(t) => self.lower_expr(&reactive_cell(e, *t)),
                        None => self.lower_expr(e),
                    },
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
            is_unsafe: false,
            this: self.f.this,
            owner: Some(sid),
            generics: Vec::new(),
            closure: None,
            defaults,
            rest: has_rest,
            c_variadic: false,
            throws,
            infer_throws: false,
            generator: false,
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
        self.enter_module(c.decl.name.span);
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
                Some(e) => match c.reactive_fields.get(&name.name) {
                    Some(t) => self.lower_expr(&reactive_cell(e, *t)),
                    None => self.lower_expr(e),
                },
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
            is_unsafe: false,
            this: None,
            owner: Some(sid),
            generics: Vec::new(),
            closure: None,
            defaults: Vec::new(),
            rest: false,
            c_variadic: false,
            throws: Vec::new(),
            infer_throws: false,
            generator: false,
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
                    match self.top_globals.get(&self.mods, &name.name).copied() {
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
                    if let ast::PatternKind::Rest(name) = &sub.kind {
                        // `[first, ...rest]`: the rest of an array (it must come last).
                        if i + 1 != elems.len() {
                            self.error(
                                codes::PATTERN_MISMATCH,
                                sub.span,
                                "`...rest` must be the last element",
                                "move it to the end",
                            );
                            continue;
                        }
                        let Some(name) = name else { continue };
                        let rest = self.method_call(
                            tmp,
                            "slice",
                            vec![ExprKind::Int(i as u64, None)],
                            sub.span,
                        );
                        let ident = ast::Pattern {
                            kind: ast::PatternKind::Ident(name.clone()),
                            span: sub.span,
                        };
                        self.destructure(&ident, rest, mutable, None, out, global);
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
            // `{ x, y: py, port = 8080 }`: fields; a default replaces a null field.
            ast::PatternKind::Struct { fields, rest, .. } => {
                if rest.is_some() {
                    self.not_implemented(span, "`...rest` in object patterns");
                }
                let tmp = self.hidden_local("<destructured>", span);
                self.f.locals[tmp.0 as usize].ty = ty;
                out.push(Stmt::Let {
                    var: Var::Local(tmp),
                    init: Some(source),
                    span,
                });
                for f in fields {
                    let object = self.expr(ExprKind::Var(Var::Local(tmp)), f.span);
                    let mut value = self.expr(
                        ExprKind::Member {
                            object: Box::new(object),
                            name: f.name.name.clone(),
                            name_span: f.name.span,
                        },
                        f.span,
                    );
                    if let Some(d) = &f.default {
                        let d = self.lower_expr(d);
                        value = self.expr(
                            ExprKind::Binary(ast::BinOp::Nullish, Box::new(value), Box::new(d)),
                            f.span,
                        );
                    }
                    let sub = f.pat.clone().unwrap_or(ast::Pattern {
                        kind: ast::PatternKind::Ident(f.name.clone()),
                        span: f.name.span,
                    });
                    self.destructure(&sub, value, mutable, None, out, global);
                }
            }
            _ => self.not_implemented(span, "this kind of destructuring pattern"),
        }
    }

    /// `local.name(args)`
    fn method_call(&mut self, local: LocalId, name: &str, args: Vec<ExprKind>, span: Span) -> Expr {
        let object = self.expr(ExprKind::Var(Var::Local(local)), span);
        let callee = self.expr(
            ExprKind::Member {
                object: Box::new(object),
                name: name.into(),
                name_span: span,
            },
            span,
        );
        let args = args.into_iter().map(|a| self.expr(a, span)).collect();
        self.expr(
            ExprKind::Call {
                callee: Box::new(callee),
                args,
                type_args: Vec::new(),
            },
            span,
        )
    }

    /// `[...a, x, ...b]` → `a.concat([x]).concat(b)` (a lone spread copies: `a.slice(0)`).
    fn array_spread(&mut self, elems: &[ast::ArrayElem], span: Span) -> ExprKind {
        let mut parts: Vec<Expr> = Vec::new();
        let mut plain: Vec<Expr> = Vec::new();
        for el in elems {
            match el {
                ast::ArrayElem::Expr(x) => plain.push(self.lower_expr(x)),
                ast::ArrayElem::Spread(x) => {
                    if !plain.is_empty() {
                        let lit = self.expr(ExprKind::Array(std::mem::take(&mut plain)), span);
                        parts.push(lit);
                    }
                    parts.push(self.lower_expr(x));
                }
            }
        }
        if !plain.is_empty() {
            parts.push(self.expr(ExprKind::Array(plain), span));
        }
        let mut it = parts.into_iter();
        let first = it.next().expect("a spread");
        let call = |r: &mut Resolver, object: Expr, name: &str, args: Vec<Expr>| {
            let callee = r.expr(
                ExprKind::Member {
                    object: Box::new(object),
                    name: name.into(),
                    name_span: span,
                },
                span,
            );
            r.expr(
                ExprKind::Call {
                    callee: Box::new(callee),
                    args,
                    type_args: Vec::new(),
                },
                span,
            )
        };
        let mut acc = match it.next() {
            Some(second) => call(self, first, "concat", vec![second]),
            None => {
                let zero = self.expr(ExprKind::Int(0, None), span);
                call(self, first, "slice", vec![zero])
            }
        };
        for part in it {
            acc = call(self, acc, "concat", vec![part]);
        }
        acc.kind
    }

    /// `[a, b] = value` → `{ const t = value; a = t[0]; b = t[1] }` (the value is read first, so
    /// `[a, b] = [b, a]` swaps).
    fn assign_destructured(
        &mut self,
        target: &ast::Expr,
        value: &ast::Expr,
        span: Span,
    ) -> ExprKind {
        let ast::ExprKind::Array(elems) = &target.kind else {
            unreachable!("checked")
        };
        let v = self.lower_expr(value);
        let tmp = self.hidden_local("<destructured>", span);
        let mut stmts = vec![Stmt::Let {
            var: Var::Local(tmp),
            init: Some(v),
            span,
        }];
        for (i, el) in elems.iter().enumerate() {
            let ast::ArrayElem::Expr(x) = el else {
                self.not_implemented(span, "`...rest` in destructuring assignments");
                continue;
            };
            let Some(t) = self.lower_target(x) else {
                continue;
            };
            let object = self.expr(ExprKind::Var(Var::Local(tmp)), x.span);
            let index = self.expr(ExprKind::Int(i as u64, None), x.span);
            let elem = self.expr(
                ExprKind::Index {
                    object: Box::new(object),
                    index: Box::new(index),
                },
                x.span,
            );
            let assign = self.expr(
                ExprKind::Assign {
                    op: None,
                    target: t,
                    value: Box::new(elem),
                },
                x.span,
            );
            stmts.push(Stmt::Expr(assign));
        }
        ExprKind::Block(Block {
            stmts,
            tail: None,
            span,
        })
    }

    /// The statements that destructure renamed parameters at the start of the body.
    fn destructured_params(&mut self, params: &[(ast::Ident, ast::Pattern)]) -> Vec<Stmt> {
        let mut out = Vec::new();
        for (name, pat) in params {
            let Some(Found::Var(v)) = self.lookup(&name.name, name.span) else {
                continue;
            };
            let source = self.expr(ExprKind::Var(v), pat.span);
            self.destructure(pat, source, false, None, &mut out, false);
        }
        out
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
        // Comptime code sees the `comptime const`s of the function around it (not as
        // captures: it runs their thunks again).
        if self.comptime_depth > 0
            && let Some(f) = self.outer_comptime_const(name)
        {
            return Some(Found::Thunk(f));
        }
        // A closure sees the locals of the functions around it (capturing them).
        if let Some(id) = self.capture(name) {
            return Some(Found::Var(Var::Local(id)));
        }
        if let Some(t) = self.const_param(name) {
            return Some(Found::Const(t));
        }
        // An arrow captures `this` only for a name that is a member: capturing it for any name
        // made `comptime` code in a method (`target.os`) capture an unused `this` (#104).
        if self.f.this.is_none()
            && self.f.arrow
            && let Some(sid) = self.this_struct()
        {
            let def = &self.structs[sid.0 as usize];
            if def.field(name).is_some()
                || def.method(name).is_some()
                || def.getter(name).is_some()
                || def.setter(name).is_some()
            {
                self.capture_this();
            }
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
        if let Some(&g) = self.top_globals.get(&self.mods, name) {
            if self.f.in_entry && !self.globals_declared.contains(&g) {
                let decl = self.globals[g.0 as usize].span;
                if self.module_of(decl.file) != self.mods.cur {
                    self.diags.push(
                        Diagnostic::error(
                            codes::INIT_CYCLE,
                            format!("`{name}` isn't initialized yet: the modules' initialization forms a cycle"),
                        )
                        .primary(span, "used while its module is still waiting for this one")
                        .secondary(decl, "initialized here")
                        .note("modules that import each other may share declarations, but their top-level initializers can't depend on each other"),
                    );
                    return None;
                }
                self.use_before_decl(name, span, decl);
                return None;
            }
            return Some(Found::Var(Var::Global(g)));
        }
        if let Some(fs) = self.top_fns.get(&self.mods, name) {
            return Some(match fs.as_slice() {
                [f] => Found::Fn(*f),
                many => Found::Overloads(many.to_vec()),
            });
        }
        if let Some(b) = Builtin::from_name(name) {
            return Some(Found::Builtin(b));
        }
        // A type where a value goes: a `comptime T: type` argument (anything else is an
        // error in type checking).
        let named = ast::Expr {
            kind: ast::ExprKind::Ident(name.into()),
            span,
        };
        if let Some(t) = self.type_named(&named, "") {
            return Some(Found::Type(t));
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
        candidates.extend(self.top_globals.visible(&self.mods));
        candidates.extend(self.top_fns.visible(&self.mods));
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
            // In an arrow (an async method's body), `this` is captured on first use (#104).
            self.capture_this();
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
    /// `comptime e` (spec §11.1): `e` becomes a closure of no parameters that the compiler
    /// runs; it can't use the variables of the code around it (E0352). `comptime if` makes
    /// only the condition comptime (folding then drops the other branch).
    fn comptime(&mut self, inner: &ast::Expr, span: Span) -> Expr {
        if let ast::ExprKind::If { cond, then, else_ } = &inner.kind {
            let cond = ast::Expr {
                kind: ast::ExprKind::Comptime(cond.clone()),
                span: cond.span,
            };
            let e = ast::Expr {
                kind: ast::ExprKind::If {
                    cond: Box::new(cond),
                    then: then.clone(),
                    else_: else_.clone(),
                },
                span,
            };
            return self.lower_expr(&e);
        }
        let limit = self.comptime_limit.take();
        let body = match &inner.kind {
            ast::ExprKind::Block(b) => ast::FnBody::Block(b.clone()),
            _ => ast::FnBody::Expr(Box::new(inner.clone())),
        };
        let decl = ast::FnDecl {
            name: Some(ast::Ident {
                name: "<comptime>".into(),
                span: span.shrink_to_lo(),
            }),
            flags: ast::FnFlags {
                arrow: true,
                ..Default::default()
            },
            generics: Vec::new(),
            params: Vec::new(),
            ret: None,
            throws: None,
            where_clause: Vec::new(),
            body: Some(body),
            span,
        };
        self.comptime_depth += 1;
        let k = self.closure(&decl, span);
        self.comptime_depth -= 1;
        let ExprKind::Closure(id) = k else {
            unreachable!("a closure")
        };
        let func = self.fns[id.0 as usize].as_ref().expect("just lowered");
        let names: Vec<String> = func
            .closure
            .as_ref()
            .expect("a closure")
            .captures
            .iter()
            .map(|c| func.locals[c.inner.0 as usize].name.clone())
            .collect();
        if !names.is_empty() {
            let list: Vec<String> = names.iter().map(|n| format!("`{n}`")).collect();
            self.diags.push(
                Diagnostic::error(
                    codes::COMPTIME_CAPTURE,
                    format!(
                        "comptime code uses {} of the code around it",
                        list.join(", ")
                    ),
                )
                .primary(span, "evaluated by the compiler, before those exist")
                .help("use only globals, `comptime const`s and functions inside `comptime`"),
            );
        }
        self.expr(ExprKind::Comptime { f: id, limit }, span)
    }

    /// `comptime const x = e` as `const x = comptime e` (`None` for other statements); sets
    /// `comptime_limit` for that `comptime`.
    fn comptime_binding(&mut self, s: &ast::Stmt) -> Option<ast::Stmt> {
        let ast::StmtKind::Let {
            kind,
            pat,
            ty,
            init: Some(e),
            comptime: true,
        } = &s.kind
        else {
            return None;
        };
        self.comptime_limit = self.decorated_limit(s.span);
        let wrapped = ast::Expr {
            kind: ast::ExprKind::Comptime(Box::new(e.clone())),
            span: e.span,
        };
        Some(ast::Stmt {
            kind: ast::StmtKind::Let {
                kind: *kind,
                pat: pat.clone(),
                ty: ty.clone(),
                init: Some(wrapped),
                comptime: false,
            },
            span: s.span,
        })
    }

    /// The `@comptimeLimit(n)` on a top-level binding, if any (other decorators on bindings
    /// aren't implemented).
    fn decorated_limit(&mut self, stmt: Span) -> Option<u64> {
        let decorators = self.binding_decorators.get(&stmt)?.clone();
        let mut limit = None;
        for d in &decorators {
            let is_limit = d.path.len() == 1 && d.path[0].name == "comptimeLimit";
            if !is_limit {
                self.not_implemented(d.span, "decorators on bindings");
                continue;
            }
            match d.args.as_deref() {
                Some(
                    [
                        ast::Arg {
                            name: None,
                            spread: false,
                            value:
                                ast::Expr {
                                    kind: ast::ExprKind::Lit(ast::Lit::Int { value, .. }),
                                    ..
                                },
                        },
                    ],
                ) if *value > 0 => limit = Some(*value),
                _ => self.error(
                    codes::MISMATCHED_TYPES,
                    d.span,
                    "`@comptimeLimit` takes a positive integer literal",
                    "e.g. `@comptimeLimit(50_000_000)`",
                ),
            }
        }
        limit
    }

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
            generator: f.flags.generator,
            async_step: f.flags.is_async && f.flags.generator && f.flags.arrow,
            async_gen: f.flags.is_async
                && f.flags.generator
                && f.flags.arrow
                && f.name
                    .as_ref()
                    .is_some_and(|n| n.name.starts_with("<async* ")),
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
        if (f.flags.is_async && !self.f.async_step) || (f.flags.generator && !f.flags.arrow) {
            self.not_implemented(span, "async and generator closures");
        }
        if let Some(g) = f.generics.first() {
            self.not_implemented(g.span, "generic closures");
        }
        let mut params = Vec::new();
        let (f_params, destructured) = split_destructured(&f.params);
        for p in &f_params {
            let ast::PatternKind::Ident(pname) = &p.pat.kind else {
                unreachable!("destructured parameters were renamed");
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
        let pre = self.destructured_params(&destructured);
        let mut body = match &f.body {
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
        body.stmts.splice(0..0, pre);
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
            is_unsafe: false,
            this: st.this,
            owner: st.owner,
            generics: Vec::new(),
            closure: Some(ClosureInfo {
                parent,
                captures: st.captures,
            }),
            defaults: Vec::new(),
            rest: false,
            c_variadic: false,
            throws: Vec::new(),
            infer_throws: true,
            generator: f.flags.generator,
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
        let sid = self.this_struct()?;
        let def = &self.structs[sid.0 as usize];
        (def.field(name).is_some() || def.setter(name).is_some())
            .then(|| Found::Member(name.to_string()))
    }

    /// Whether `this`'s class has a method or getter `name` (its own or inherited), so a bare
    /// `build()` in a method calls it rather than meaning the build constant (#104).
    fn this_has_method(&self, name: &str) -> bool {
        if self
            .f
            .scopes
            .iter()
            .any(|s| s.names.contains_key(name) || s.later.contains_key(name))
        {
            return false;
        }
        self.this_struct().is_some_and(|sid| {
            let def = &self.structs[sid.0 as usize];
            def.method(name).is_some() || def.getter(name).is_some()
        })
    }

    /// The class `this` is in the current function: its own, or (in an arrow, such as an async
    /// method's body, which captures `this` only when it's used) the enclosing method's, found
    /// the way `capture_this` finds it, without capturing (#104).
    fn this_struct(&self) -> Option<StructId> {
        if let Some((_, sid)) = self.f.this {
            return Some(sid);
        }
        if !self.f.arrow {
            return None;
        }
        let n = self.outer_fns.len();
        for lvl in 1..=n {
            let st = &self.outer_fns[n - lvl];
            if let Some((_, sid)) = st.this {
                return Some(sid);
            }
            if !(st.is_closure && st.arrow) {
                return None;
            }
        }
        None
    }

    fn resolve_var(&mut self, name: &str, span: Span) -> Option<Var> {
        match self.lookup(name, span)? {
            Found::Var(v) => Some(v),
            Found::Fn(_)
            | Found::Overloads(_)
            | Found::Builtin(_)
            | Found::Member(_)
            | Found::Const(_)
            | Found::Type(_)
            | Found::Thunk(_) => {
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

    /// Assigning a `const` is an error, but assigning one that may hold a `Reactive<T>` (one
    /// declared with that type, or initialized by a call) writes the cell (spec §16.2): only
    /// its type tells, so the type checker decides (`check_assignable`).
    fn check_mutable(&mut self, var: Var, span: Span) {
        let b = match var {
            Var::Local(l) => &self.f.locals[l.0 as usize],
            Var::Global(g) => &self.globals[g.0 as usize],
        };
        let cell_type = matches!(&b.ty, Some(Ty::Class(_, name, _)) if &**name == "Reactive");
        if b.mutable || cell_type || self.maybe_cells.contains(&b.span) {
            return;
        }
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

    /// `Reactive<t>` (spec §16.1).
    fn reactive_ty(&self, t: Ty) -> Ty {
        match self
            .top_structs
            .prelude()
            .find(|(n, _)| *n == "Reactive")
            .map(|(_, v)| *v)
        {
            Some(sid) => Ty::Class(sid, "Reactive".into(), std::sync::Arc::from([t])),
            None => Ty::Error,
        }
    }

    /// `reactive x: T = v` (spec §16.1) as `const x: Reactive<T> = __reactive<T>(v)`.
    fn reactive_binding(&mut self, s: &ast::Stmt) -> Option<ast::Stmt> {
        let ast::StmtKind::Let {
            kind: ast::BindingKind::Reactive,
            pat,
            ty,
            init,
            comptime,
        } = &s.kind
        else {
            return None;
        };
        if !matches!(pat.kind, ast::PatternKind::Ident(_)) {
            self.error(
                codes::REACTIVE,
                pat.span,
                "a `reactive` binding can't be a destructuring pattern",
                "not a name",
            );
        }
        if *comptime {
            self.error(
                codes::REACTIVE,
                s.span,
                "a `reactive` binding can't be `comptime`",
                "here",
            );
        }
        let Some(init) = init else {
            self.error(
                codes::REACTIVE,
                s.span,
                "a `reactive` binding needs an initial value",
                "no `= value`",
            );
            return Some(ast::Stmt {
                kind: ast::StmtKind::Let {
                    kind: ast::BindingKind::Const,
                    pat: pat.clone(),
                    ty: ty.as_ref().map(reactive_type),
                    init: None,
                    comptime: false,
                },
                span: s.span,
            });
        };
        Some(ast::Stmt {
            kind: ast::StmtKind::Let {
                kind: ast::BindingKind::Const,
                pat: pat.clone(),
                ty: ty.as_ref().map(reactive_type),
                init: Some(reactive_cell(init, ty.as_ref())),
                comptime: false,
            },
            span: s.span,
        })
    }

    // ------------------------------------------------------------ types

    pub(crate) fn resolve_type(&mut self, t: &ast::Type) -> Ty {
        let ty = self.resolve_type_here(t);
        self.reject_nested_reactive(t, &ty);
        if let ast::TypeKind::Path { path, .. } = &t.kind
            && let [name] = path.as_slice()
            && let Some(decl) = self.type_decl(&name.name)
        {
            self.name_refs.push((name.span, decl));
        }
        ty
    }

    /// `Reactive<T>` is a binding's or a function result's type only (spec §16.1): not an
    /// array element, a tuple element, a union member, a nullable or a type argument (#17).
    fn reject_nested_reactive(&mut self, t: &ast::Type, ty: &Ty) {
        if self.mods.cur == PRELUDE_MODULE {
            return;
        }
        let is_cell = |t: &Ty| matches!(t, Ty::Class(_, n, _) if &**n == "Reactive");
        let inner: Vec<&Ty> = match ty {
            Ty::Array(e) | Ty::Fixed(e, _) | Ty::Nullable(e) => vec![&**e],
            Ty::Tuple(ts) => ts.iter().collect(),
            Ty::Union(_, ms) => ms.iter().collect(),
            Ty::Class(_, n, args) | Ty::Struct(_, n, args) if &**n != "Reactive" => {
                args.iter().collect()
            }
            _ => Vec::new(),
        };
        if inner.into_iter().any(is_cell) {
            self.error(
                codes::REACTIVE,
                t.span,
                format!("`Reactive<T>` can't be part of the type `{ty}`"),
                "a reactive cell here",
            );
        }
    }

    /// Where the top-level `name` in scope is declared (after [`Resolver::program`]).
    fn top_level_decl(&self, name: &str, p: &Program) -> Option<Span> {
        if let Some(sid) = self.top_structs.get(&self.mods, name) {
            return Some(p.structs[sid.0 as usize].name_span);
        }
        if let Some(eid) = self.top_enums.get(&self.mods, name) {
            return Some(p.enums[eid.0 as usize].name_span);
        }
        if let Some(fs) = self.top_fns.get(&self.mods, name) {
            return Some(p.fns[fs.first()?.0 as usize].name_span);
        }
        if let Some(g) = self.top_globals.get(&self.mods, name) {
            return Some(p.globals[g.0 as usize].span);
        }
        let key = self
            .mods
            .resolve(name, &|k| self.aliases.map.contains_key(k))?;
        Some(self.aliases.map[&key].1)
    }

    /// Where the type `name` in scope is declared (for the language server's index): a
    /// type parameter, an alias, a struct, class, interface or enum.
    fn type_decl(&self, name: &str) -> Option<Span> {
        if Ty::from_name(name).is_some() {
            return None;
        }
        let alias = |r: &Self| {
            let key = r.mods.resolve(name, &|k| r.aliases.map.contains_key(k))?;
            Some(r.aliases.map[&key].1)
        };
        if self.f.type_params.iter().any(|(n, _)| n == name) {
            return None;
        }
        if self.aliases.outside_prelude(&self.mods, name) {
            return alias(self);
        }
        if let Some(sid) = self.top_structs.get(&self.mods, name) {
            return Some(self.structs[sid.0 as usize].name_span);
        }
        if let Some(eid) = self.top_enums.get(&self.mods, name) {
            return Some(self.enums[eid.0 as usize].name_span);
        }
        alias(self)
    }

    fn resolve_type_here(&mut self, t: &ast::Type) -> Ty {
        if let ast::TypeKind::Path { path, args } = &t.kind
            && let [ns, name] = path.as_slice()
            && let Some(target) = self
                .mods
                .namespaces
                .get(&self.mods.cur)
                .and_then(|n| n.get(&ns.name))
                .copied()
        {
            return self.namespace_type(target, name, args, t.span);
        }
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
            // A `comptime` parameter as a type argument (`zeros<N>()`).
            ast::TypeKind::Path { path, args }
                if path.len() == 1
                    && args.is_empty()
                    && !self.f.type_params.iter().any(|(n, _)| *n == path[0].name)
                    && self.const_param(&path[0].name).is_some() =>
            {
                self.const_param(&path[0].name).expect("checked")
            }
            ast::TypeKind::Path { path, args } if path.len() == 1 && args.is_empty() => {
                match Ty::from_name(&path[0].name) {
                    Some(ty) => ty,
                    // A program's own alias shadows a core library type of the same name.
                    None if self.aliases.outside_prelude(&self.mods, &path[0].name) => {
                        self.alias(&path[0].name, t.span)
                    }
                    None if self.top_structs.contains_key(&self.mods, &path[0].name) => {
                        let sid = *self
                            .top_structs
                            .get(&self.mods, &path[0].name)
                            .expect("checked");
                        if self.structs[sid.0 as usize].generics.is_empty() {
                            self.struct_ty(sid)
                        } else {
                            self.generic_ty(sid, Vec::new(), t.span)
                        }
                    }
                    None if self.top_enums.contains_key(&self.mods, &path[0].name) => self.enum_ty(
                        *self
                            .top_enums
                            .get(&self.mods, &path[0].name)
                            .expect("checked"),
                    ),
                    None if self.aliases.contains_key(&self.mods, &path[0].name) => {
                        self.alias(&path[0].name, t.span)
                    }
                    // `zeros<k>()` with a variable `k`.
                    None if self.is_value_name(&path[0].name) => {
                        self.not_constant(t.span);
                        Ty::Error
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
            // A raw pointer (spec §9.5); represented like the internal references.
            ast::TypeKind::Path { path, args }
                if path.len() == 1
                    && path[0].name == "ptr"
                    && args.len() == 1
                    && !self.top_structs.contains_key(&self.mods, "ptr") =>
            {
                Ty::Ref(Box::new(self.resolve_type(&args[0])))
            }
            ast::TypeKind::Path { path, args }
                if path.len() == 1 && self.top_structs.contains_key(&self.mods, &path[0].name) =>
            {
                let sid = *self
                    .top_structs
                    .get(&self.mods, &path[0].name)
                    .expect("checked");
                let args: Vec<Ty> = args.iter().map(|a| self.resolve_type(a)).collect();
                self.generic_ty(sid, args, t.span)
            }
            ast::TypeKind::Array(elem) => Ty::Array(Box::new(self.resolve_type(elem))),
            ast::TypeKind::Fixed { elem, len } => {
                let elem = self.resolve_type(elem);
                match self.const_len(len) {
                    Some(Ok(n)) => Ty::Fixed(Box::new(elem), n),
                    Some(Err(p)) => Ty::FixedN(Box::new(elem), Box::new(p)),
                    None => Ty::Error,
                }
            }
            ast::TypeKind::Tuple(elems) => {
                Ty::Tuple(elems.iter().map(|t| self.resolve_type(t)).collect())
            }
            ast::TypeKind::Nullable(inner) => self.resolve_type(inner).nullable(),
            ast::TypeKind::Lit(ast::Lit::Null) => Ty::Null,
            // A `comptime` argument: `zeros<4>()`, `zeros<N>()`.
            ast::TypeKind::Const(e) => self.const_arg(e).unwrap_or(Ty::Error),
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
                if extern_abi.as_deref().is_some_and(|a| a != "C") {
                    self.not_implemented(
                        t.span,
                        "`extern` function types for ABIs other than \"C\"",
                    );
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
                if extern_abi.is_some() {
                    // A C function pointer: a pointer to the function (decision 261).
                    return Ty::Ref(Box::new(Ty::Fn(ps.into(), Box::new(ret), ts.into())));
                }
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
        let Some(key) = self
            .mods
            .resolve(name, &|k| self.aliases.map.contains_key(k))
        else {
            return Ty::Error;
        };
        if let Some(t) = self.alias_cache.get(&key) {
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
        let (ty, _) = self.aliases.map[&key].clone();
        self.resolving_aliases.push(name.to_string());
        // An alias means what its own module's names mean.
        let saved = std::mem::replace(&mut self.mods.cur, key.0);
        let t = match &ty.kind {
            ast::TypeKind::Union(members) => self.union_type(members, Some(name), ty.span),
            ast::TypeKind::Distinct(base) => {
                let b = self.resolve_type(base);
                self.distincts += 1;
                Ty::Distinct(self.distincts, name.into(), Box::new(b))
            }
            _ => self.resolve_type(&ty),
        };
        self.mods.cur = saved;
        self.resolving_aliases.pop();
        self.alias_cache.insert(key, t.clone());
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
                        weak: false,
                        json_name: None,
                    }],
                    has_parens: true,
                })
                .collect(),
            origin: EnumOrigin::Union(members.clone()),
            throwable: false,
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
            throwable: false,
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
                weak: false,
                json_name: None,
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
                    weak: false,
                    json_name: None,
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

    /// The length of a fixed array: an integer literal, or (`Err`) a `comptime` parameter.
    fn const_len(&mut self, e: &ast::Expr) -> Option<Result<u64, Ty>> {
        match &strip_parens(e).kind {
            ast::ExprKind::Lit(ast::Lit::Int { value, .. }) => Some(Ok(*value)),
            ast::ExprKind::Ident(name) if self.const_param(name).is_some() => {
                let p = self.const_param(name).expect("checked");
                if let Ty::Param(o, i, _) = &p {
                    self.len_params.insert((*o, *i));
                }
                Some(Err(p))
            }
            _ => {
                self.not_implemented(
                    e.span,
                    "array lengths other than integer literals and `comptime` parameters",
                );
                None
            }
        }
    }

    /// The thunk of the local `comptime const` `name` of the closest function around this
    /// one that declares `name` (`None` if that's another kind of local).
    fn outer_comptime_const(&self, name: &str) -> Option<FnId> {
        for st in self.outer_fns.iter().rev() {
            if let Some(id) = st
                .scopes
                .iter()
                .rev()
                .find_map(|sc| sc.names.get(name).copied())
            {
                return st.comptime_consts.get(&id).copied();
            }
            if !st.is_closure {
                break;
            }
        }
        None
    }

    /// The `comptime` parameter `name` of this function or one around it (`Ty::Param`).
    fn const_param(&self, name: &str) -> Option<Ty> {
        std::iter::once(&self.f)
            .chain(self.outer_fns.iter().rev())
            .find_map(|st| {
                st.const_params
                    .iter()
                    .rev()
                    .find(|(n, _)| n == name)
                    .map(|(_, t)| t.clone())
            })
    }

    /// A `comptime` type argument (`zeros<4>()`): a literal (a literal type such as `4`) or
    /// a `comptime` parameter of the code around it.
    fn const_arg(&mut self, e: &ast::Expr) -> Option<Ty> {
        let e = strip_parens(e);
        let lit = match &e.kind {
            ast::ExprKind::Lit(l) => lit_val(l),
            ast::ExprKind::Unary {
                op: ast::UnOp::Neg,
                expr,
            } => match &strip_parens(expr).kind {
                ast::ExprKind::Lit(ast::Lit::Int {
                    value,
                    suffix: None,
                }) => Some((Ty::INT, LitVal::Int(-(*value as i128)))),
                _ => None,
            },
            ast::ExprKind::Ident(name) => {
                return self.const_param(name).or_else(|| {
                    self.not_constant(e.span);
                    None
                });
            }
            _ => None,
        };
        match lit {
            Some((base, v)) => Some(Ty::Lits(Box::new(base), vec![v].into())),
            None => {
                self.not_constant(e.span);
                None
            }
        }
    }

    fn not_constant(&mut self, span: Span) {
        self.diags.push(
            Diagnostic::error(
                codes::COMPTIME_ARG,
                "a `comptime` argument must be a constant",
            )
            .primary(span, "not known at compile time")
            .help("pass a literal, or a `comptime` parameter of the code around it"),
        );
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
            if let ast::StmtKind::Using {
                is_await,
                name,
                init,
            } = &s.kind
                && (!*is_await || self.f.async_step || (self.f.in_entry && !self.f.is_closure))
            {
                // `using x = init; rest` → `const x = init; try { rest } finally { x?.dispose() }`:
                // disposed on every way out, the latest first.
                let binding = ast::Stmt {
                    kind: ast::StmtKind::Let {
                        kind: ast::BindingKind::Const,
                        pat: ast::Pattern {
                            kind: ast::PatternKind::Ident(name.clone()),
                            span: name.span,
                        },
                        ty: None,
                        init: Some(init.clone()),
                        comptime: false,
                    },
                    span: s.span,
                };
                self.lower_stmt(&binding, &mut out, None);
                let rest = self.block_stmts(&stmts[i + 1..], span);
                let dispose = ast::Expr {
                    kind: ast::ExprKind::Call {
                        callee: Box::new(ast::Expr {
                            kind: ast::ExprKind::Member {
                                object: Box::new(ast::Expr {
                                    kind: ast::ExprKind::Ident(name.name.clone()),
                                    span: name.span,
                                }),
                                prop: ast::Ident {
                                    name: "dispose".into(),
                                    span: name.span,
                                },
                                optional: true,
                            },
                            span: name.span,
                        }),
                        type_args: Vec::new(),
                        args: Vec::new(),
                    },
                    span: name.span,
                };
                // `await using`: `if (x != null) await x.disposeAsync()`.
                let dispose = if *is_await {
                    let span = name.span;
                    let call = build::call(
                        build::member(build::ident(&name.name, span), "disposeAsync", span),
                        Vec::new(),
                        span,
                    );
                    let not_null = build::e(
                        ast::ExprKind::Binary {
                            op: ast::BinOp::Ne,
                            lhs: Box::new(build::ident(&name.name, span)),
                            rhs: Box::new(build::e(ast::ExprKind::Lit(ast::Lit::Null), span)),
                        },
                        span,
                    );
                    build::e(
                        ast::ExprKind::If {
                            cond: Box::new(not_null),
                            then: Box::new(build::e(ast::ExprKind::Await(Box::new(call)), span)),
                            else_: None,
                        },
                        span,
                    )
                } else {
                    dispose
                };
                let dispose = self.lower_expr(&dispose);
                let fin = self.expr(
                    ExprKind::Block(Block {
                        stmts: vec![Stmt::Expr(dispose)],
                        tail: None,
                        span: name.span,
                    }),
                    name.span,
                );
                let body = self.expr(ExprKind::Block(rest), span);
                tail = Some(Box::new(self.expr(
                    ExprKind::Try {
                        body: Box::new(body),
                        catches: Vec::new(),
                        finally: Some(Box::new(fin)),
                    },
                    s.span,
                )));
                break;
            }
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
                kind: ast::BindingKind::Reactive,
                ..
            } => {
                if let Some(st) = self.reactive_binding(s) {
                    self.lower_stmt(&st, out, tail);
                }
            }
            ast::StmtKind::Let {
                kind,
                pat,
                ty,
                init,
                comptime,
            } => {
                if *comptime && let Some(st) = self.comptime_binding(s) {
                    self.lower_stmt(&st, out, tail);
                    self.comptime_limit = None;
                    return;
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
                if *kind == ast::BindingKind::Const
                    && matches!(
                        init.as_ref().map(|e| &strip_parens(e).kind),
                        Some(ast::ExprKind::Call { .. })
                    )
                {
                    self.maybe_cells.insert(name.span);
                }
                let init = init.as_ref().map(|e| self.lower_expr(e));
                let id = self.declare_block_local(name, *kind == ast::BindingKind::Let, ty);
                // A local `comptime const`: later comptime code runs its thunk again.
                if *kind == ast::BindingKind::Const
                    && let Some(Expr {
                        kind: ExprKind::Comptime { f, .. },
                        ..
                    }) = &init
                {
                    self.f.comptime_consts.insert(id, *f);
                }
                out.push(Stmt::Let {
                    var: Var::Local(id),
                    init,
                    span: s.span,
                });
            }
            ast::StmtKind::Using { is_await: true, .. } => self.error(
                codes::AWAIT_OUTSIDE_ASYNC,
                s.span,
                "`await using` outside an async function",
                "not inside an `async function`",
            ),
            ast::StmtKind::Using { .. } => {
                self.not_implemented(s.span, "`using` at the top level of a module")
            }
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
            return self.for_await(kind, pat, iter, body, span);
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

    /// The module an identifier names through `import * as`, unless something closer shadows it.
    fn namespace(&self, object: &ast::Expr) -> Option<u32> {
        let ast::ExprKind::Ident(n) = &object.kind else {
            return None;
        };
        if self.f.scopes.iter().any(|s| s.names.contains_key(n))
            || self
                .outer_fns
                .iter()
                .any(|o| o.scopes.iter().any(|s| s.names.contains_key(n)))
        {
            return None;
        }
        self.mods.namespaces.get(&self.mods.cur)?.get(n).copied()
    }

    /// The key an exported name of module `target` refers to (`None` with an error reported).
    fn exported_key(
        &mut self,
        target: u32,
        name: &Ident,
        exists: &dyn Fn(&Resolver, &(u32, String)) -> bool,
    ) -> Option<(u32, String)> {
        let local = self
            .mods
            .exports
            .get(&target)
            .and_then(|e| e.get(&name.name))
            .cloned();
        let Some(local) = local else {
            let m = self.mods.names.get(&target).cloned().unwrap_or_default();
            self.error(
                codes::NOT_EXPORTED,
                name.span,
                format!("`{}` isn't exported by `{m}`", name.name),
                "not exported",
            );
            return None;
        };
        let found = self
            .mods
            .resolve_in(target, &local, &|k| exists(self, k), 0);
        if found.is_none() {
            self.error(
                codes::UNKNOWN_NAME,
                name.span,
                format!("cannot find `{}`", name.name),
                "not found",
            );
        }
        found
    }

    /// `ns.name` as a value: an exported global or function of the module.
    fn namespace_value(&mut self, target: u32, prop: &Ident) -> ExprKind {
        let exists = |r: &Resolver, k: &(u32, String)| {
            r.top_globals.map.contains_key(k) || r.top_fns.map.contains_key(k)
        };
        let Some(k) = self.exported_key(target, prop, &exists) else {
            return ExprKind::Error;
        };
        if let Some(&g) = self.top_globals.map.get(&k) {
            return ExprKind::Var(Var::Global(g));
        }
        match self.top_fns.map.get(&k).map(Vec::as_slice) {
            Some([f]) => ExprKind::Fn(*f),
            Some(fs) => ExprKind::Overloaded(fs.to_vec()),
            None => ExprKind::Error,
        }
    }

    /// `ns.Type` in a type annotation.
    fn namespace_type(&mut self, target: u32, name: &Ident, args: &[ast::Type], span: Span) -> Ty {
        let exists = |r: &Resolver, k: &(u32, String)| {
            r.top_structs.map.contains_key(k) || r.top_enums.map.contains_key(k)
        };
        let Some(k) = self.exported_key(target, name, &exists) else {
            return Ty::Error;
        };
        if let Some(&sid) = self.top_structs.map.get(&k) {
            let args: Vec<Ty> = args.iter().map(|a| self.resolve_type(a)).collect();
            return self.generic_ty(sid, args, span);
        }
        match self.top_enums.map.get(&k) {
            Some(&e) => self.enum_ty(e),
            None => Ty::Error,
        }
    }

    fn lower_expr(&mut self, e: &ast::Expr) -> Expr {
        use ast::ExprKind as A;
        let span = e.span;
        if let Some(base) = opt_link_base(e) {
            return self.opt_chain(e, base);
        }
        // `geo.dot` with `import * as geo from "./geometry"`.
        if let A::Member { object, prop, .. } = &e.kind
            && let Some(target) = self.namespace(object)
        {
            let kind = self.namespace_value(target, prop);
            return self.expr(kind, span);
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
                ast::Lit::Regex { pattern, flags } => {
                    // Checked here; at runtime `RegExp.__literal(pattern, flags)` (§13.7).
                    if let Err(msg) = crate::regex::validate(pattern, flags) {
                        self.diags.push(
                            Diagnostic::error(codes::INVALID_REGEX, "invalid regular expression")
                                .primary(span, msg),
                        );
                        return self.expr(ExprKind::Error, span);
                    }
                    let call = build::call(
                        build::member(build::ident("RegExp", span), "__literal", span),
                        vec![
                            build::e(ast::ExprKind::Lit(ast::Lit::Str(pattern.clone())), span),
                            build::e(ast::ExprKind::Lit(ast::Lit::Str(flags.clone())), span),
                        ],
                        span,
                    );
                    self.cond_scope = cond;
                    return self.lower_expr(&call);
                }
            },
            // `target` and `build` (unless the program has its own): facts about the build,
            // constants wherever they're used (spec §11.1, §15.4).
            A::Ident(name)
                if (name == "target" || name == "build")
                    && !self.is_value_name(name)
                    && !self.top_fns.contains_key(&self.mods, name)
                    && self.peek_member(name).is_none()
                    && !self.this_has_method(name) =>
            {
                let f = if name == "target" {
                    "__target"
                } else {
                    "__build"
                };
                let call = build::call(build::ident(f, span), Vec::new(), span);
                let e = ast::Expr {
                    kind: A::Comptime(Box::new(call)),
                    span,
                };
                return self.lower_expr(&e);
            }
            A::Ident(name) => match self.lookup(name, span) {
                Some(Found::Const(t)) => ExprKind::ConstParam(t),
                Some(Found::Type(t)) => ExprKind::TypeValue(t),
                Some(Found::Thunk(f)) => ExprKind::Comptime { f, limit: None },
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
            A::Yield {
                expr: Some(inner),
                delegate: false,
            } if self.f.async_gen => {
                // `{ __it.__put(v); yield __it.__pause() }`
                let put = build::call(
                    build::member(build::ident("__it", span), "__put", span),
                    vec![(**inner).clone()],
                    span,
                );
                let pause = build::call(
                    build::member(build::ident("__it", span), "__pause", span),
                    Vec::new(),
                    span,
                );
                let put = self.lower_expr(&put);
                let pause = self.lower_expr(&pause);
                let y = self.expr(ExprKind::Yield(Box::new(pause)), span);
                ExprKind::Block(Block {
                    stmts: vec![Stmt::Expr(put), Stmt::Expr(y)],
                    tail: None,
                    span,
                })
            }
            A::Yield {
                expr: Some(inner),
                delegate: false,
            } if self.f.generator => ExprKind::Yield(Box::new(self.lower_expr(inner))),
            A::Yield { .. } if !self.f.generator => {
                self.error(
                    codes::UNKNOWN_NAME,
                    span,
                    "`yield` outside a generator",
                    "only `function*` bodies can `yield`",
                );
                ExprKind::Error
            }
            A::Yield { .. } => {
                self.not_implemented(span, "`yield*` and `yield` without a value");
                ExprKind::Error
            }
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
            A::Assign {
                op: None,
                target,
                value,
            } if matches!(target.kind, A::Array(_)) => {
                self.assign_destructured(target, value, span)
            }
            // `self.onmessage = f` in a worker module (`self` from `std/worker`): registers `f`
            // for the parent's messages, typed by `f`'s parameter.
            A::Assign {
                op: None,
                target,
                value,
            } if matches!(&target.kind, A::Member { object, prop, .. }
                if prop.name == "onmessage"
                    && matches!(&object.kind, A::Ident(n) if n == "self"
                        && !self.f.scopes.iter().any(|s| s.names.contains_key("self"))
                        && self.top_globals.contains_key(&self.mods, "self"))) =>
            {
                let c = build::call(
                    build::ident("__workerOnMessage", span),
                    vec![(**value).clone()],
                    span,
                );
                return self.lower_expr(&c);
            }
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
            A::Call { callee, args, .. }
                if matches!(&callee.kind, A::Member { object, prop, .. }
                    if prop.name == "fromC" && matches!(&object.kind, A::Ident(n) if n == "string")) =>
            {
                // `string.fromC(p)` (spec §12.1): the runtime copies the C string.
                let f = self
                    .top_fns
                    .prelude()
                    .find(|(n, _)| *n == "pico_str_from_c")
                    .map(|(_, ids)| ids[0])
                    .expect("in the core library");
                let callee = self.expr(ExprKind::Fn(f), callee.span);
                let args = self.call_args(args);
                ExprKind::Call {
                    callee: Box::new(callee),
                    args,
                    type_args: Vec::new(),
                }
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
            // `T.fields`, `Point.name`: facts about a type.
            A::Member { object, prop, .. }
                if TypeMember::from_name(&prop.name).is_some()
                    && self.type_named(object, &prop.name).is_some() =>
            {
                let ty = self.type_named(object, &prop.name).expect("checked");
                let member = TypeMember::from_name(&prop.name).expect("checked");
                let elem = match member {
                    TypeMember::Fields => self.prelude_struct_ty("FieldInfo"),
                    TypeMember::Methods => self.prelude_struct_ty("MethodInfo"),
                    TypeMember::Name | TypeMember::Kind => Ty::Str,
                };
                ExprKind::TypeInfo { ty, member, elem }
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
                        .get(&self.mods, n)
                        .copied()
                        .filter(|id| self.structs[id.0 as usize].is_class),
                    _ => None,
                };
                if let Some(c) = class {
                    let decl = self.structs[c.0 as usize].name_span;
                    self.name_refs.push((callee.span, decl));
                }
                let Some(class) = class else {
                    self.error(
                        codes::UNKNOWN_TYPE,
                        callee.span,
                        "`new` needs a class",
                        "not a class",
                    );
                    return self.error_expr(span);
                };
                // `new Worker<…>("./job")`: the worker module's id (spec §10.2), as
                // `new Worker(id, 0)`; `new WorkerPool("./job", size)` likewise.
                let worker_arg;
                let class_name = self.structs[class.0 as usize].name.clone();
                let args = match args.first().map(|a| &a.value.kind) {
                    Some(ast::ExprKind::Lit(ast::Lit::Str(spec)))
                        if class_name == "Worker" || class_name == "WorkerPool" =>
                    {
                        let Some(&id) = self.worker_ids.get(&(span.file, spec.clone())) else {
                            self.error(
                                codes::UNKNOWN_MODULE,
                                args[0].value.span,
                                format!("cannot find worker module `{spec}`"),
                                "write the module path as a literal right in `new Worker(…)`",
                            );
                            return self.error_expr(span);
                        };
                        let mut a = args.clone();
                        a[0].value.kind = ast::ExprKind::Lit(ast::Lit::Int {
                            value: u64::from(id),
                            suffix: None,
                        });
                        if class_name == "Worker" {
                            let mut kind = a[0].clone();
                            kind.value.kind = ast::ExprKind::Lit(ast::Lit::Int {
                                value: 0,
                                suffix: None,
                            });
                            a.push(kind);
                        }
                        worker_arg = a;
                        &worker_arg[..]
                    }
                    _ => &args[..],
                };
                let out = self.call_args(args);
                // `new Worker(f)`: an inline worker (spec §10.4), whose function must not
                // capture anything (it runs in another thread).
                if class_name == "Worker" && out.len() == 1 {
                    self.uses_inline_workers = true;
                    self.check_capture_free(&out[0]);
                }
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
            } if self.intrinsic(callee).is_some() => {
                let which = self.intrinsic(callee).expect("checked");
                let (want_types, want_args) = match which {
                    "ptr.of" | "ptr.bytes" => (0, 1),
                    "unsafe.cast" => (1, 1),
                    "comptime.readFile" => (0, 1),
                    "__raw_arena_alloc" => (1, 2),
                    "__throw_unchecked" | "__exit_uncaught" => (0, 1),
                    "__worker_take" => (1, 1),
                    _ => (1, 0),
                };
                if type_args.len() != want_types || args.len() != want_args {
                    self.error(
                        codes::WRONG_ARG_COUNT,
                        span,
                        format!(
                            "`{which}` takes {want_types} type argument{} and {want_args} argument{}",
                            if want_types == 1 { "" } else { "s" },
                            if want_args == 1 { "" } else { "s" }
                        ),
                        "wrong number of arguments",
                    );
                    return self.error_expr(span);
                }
                let ty = type_args.first().map(|t| self.resolve_type(t));
                if which == "ptr.bytes" {
                    // `ptr.bytes(view)`: the core library's `unsafe` `__ptrBytes` (#83).
                    let call = ast::Expr {
                        kind: ast::ExprKind::Call {
                            callee: Box::new(build::ident("__ptrBytes", span)),
                            type_args: Vec::new(),
                            args: args.clone(),
                        },
                        span,
                    };
                    return self.lower_expr(&call);
                }
                if which == "comptime.readFile" {
                    if self.comptime_depth == 0 {
                        self.error(
                            codes::COMPTIME_ONLY,
                            span,
                            "`comptime.readFile` outside comptime code",
                            "only available inside `comptime`",
                        );
                    }
                    let callee = self.expr(ExprKind::Builtin(Builtin::ComptimeReadFile), span);
                    let arg = self.lower_expr(&args[0].value);
                    return self.expr(
                        ExprKind::Call {
                            callee: Box::new(callee),
                            args: vec![arg],
                            type_args: Vec::new(),
                        },
                        span,
                    );
                }
                if which == "__raw_arena_alloc" {
                    let arena = Box::new(self.lower_expr(&args[0].value));
                    let count = Box::new(self.lower_expr(&args[1].value));
                    return self.expr(
                        ExprKind::RawArenaAlloc {
                            ty: ty.expect("checked"),
                            arena,
                            count,
                        },
                        span,
                    );
                }
                let arg = args.first().map(|a| Box::new(self.lower_expr(&a.value)));
                match which {
                    "ptr.of" => ExprKind::PtrOf(arg.expect("checked")),
                    "__worker_take" => ExprKind::WorkerTake {
                        ty: ty.expect("checked"),
                        handle: arg.expect("checked"),
                    },
                    "__throw_unchecked" => ExprKind::ThrowUnchecked(arg.expect("checked")),
                    "__exit_uncaught" => ExprKind::ExitUncaught(arg.expect("checked")),
                    "unsafe.cast" => ExprKind::BitCast(arg.expect("checked"), ty.expect("checked")),
                    _ => ExprKind::SizeOf {
                        ty: ty.expect("checked"),
                        align: which == "alignof",
                    },
                }
            }
            // `reflect.typeOf(v)` / `reflect.typeOf<T>()` (`std/reflect`): the core library's
            // `__reflectTypeOf` / `__reflectType`, kept at this call so its module decides
            // which private fields show (spec §11.3).
            A::Call {
                callee,
                type_args,
                args,
            } if self.reflect_call(callee) => {
                let (name, want_types, want_args) = if args.is_empty() {
                    ("__reflectType", 1, 0)
                } else {
                    ("__reflectTypeOf", type_args.len().min(1), 1)
                };
                if type_args.len() != want_types || args.len() != want_args {
                    self.error(
                        codes::WRONG_ARG_COUNT,
                        span,
                        "`reflect.typeOf` takes a value or a type argument: `reflect.typeOf(v)`, `reflect.typeOf<T>()`",
                        "wrong number of arguments",
                    );
                    return self.error_expr(span);
                }
                let f = self
                    .top_fns
                    .prelude()
                    .find(|(n, _)| *n == name)
                    .map(|(_, ids)| ids[0])
                    .expect("in the core library");
                // (For the language server: `reflect` and `typeOf` are `__reflect`'s.)
                if let ast::ExprKind::Member { object, prop, .. } = &callee.kind
                    && let Some(id) = self
                        .top_structs
                        .prelude()
                        .find(|(n, _)| *n == "__reflect")
                        .map(|(_, id)| *id)
                {
                    let decl = self.structs[id.0 as usize].name_span;
                    self.name_refs.push((object.span, decl));
                    self.name_refs.push((prop.span, decl));
                }
                let type_args = type_args.iter().map(|t| self.resolve_type(t)).collect();
                let callee = self.expr(ExprKind::Fn(f), callee.span);
                let args = self.call_args(args);
                ExprKind::Call {
                    callee: Box::new(callee),
                    args,
                    type_args,
                }
            }
            A::Call {
                callee,
                type_args,
                args,
            } => {
                let type_args = type_args.iter().map(|t| self.resolve_type(t)).collect();
                let is_parallel_map = matches!(&callee.kind, A::Ident(n) if n == "parallelMap");
                let callee = self.lower_expr(callee);
                let mut out = self.call_args(args);
                // A top-level function passed where an extern function takes a C function
                // pointer: its address.
                if let ExprKind::Fn(f) = &callee.kind
                    && let Some(cfns) = self.extern_cfns.get(f)
                {
                    for (a, is_cfn) in out.iter_mut().zip(cfns) {
                        if *is_cfn && let ExprKind::Fn(g) = a.kind {
                            a.kind = ExprKind::FnAddr(g);
                        }
                    }
                }
                // `parallelMap(items, f, …)` runs `f` in inline workers: no captures.
                if is_parallel_map && let Some(f) = out.get(1) {
                    self.check_capture_free(f);
                }
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
            A::Unsafe(b) => ExprKind::Unsafe(self.block(b)),
            A::Arena(b) => {
                let first = self.f.locals.len() as u32;
                let body = self.block(b);
                ExprKind::Arena {
                    body,
                    first,
                    end: self.f.locals.len() as u32,
                }
            }
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
            // In an async function: resolve its promise, then end the step.
            A::Return(value) if self.f.async_step && !self.f.async_gen => {
                let v = value
                    .as_deref()
                    .cloned()
                    .unwrap_or_else(|| build::call(build::ident("__void", span), Vec::new(), span));
                let resolve = build::call(
                    build::member(build::ident("__p", span), "__hold", span),
                    vec![v],
                    span,
                );
                let resolve = self.lower_expr(&resolve);
                let ret = self.expr(ExprKind::Return(None), span);
                ExprKind::Block(Block {
                    stmts: vec![Stmt::Expr(resolve), Stmt::Expr(ret)],
                    tail: None,
                    span,
                })
            }
            A::Return(value) => {
                ExprKind::Return(value.as_ref().map(|v| Box::new(self.lower_expr(v))))
            }
            A::Await(inner) => self.lower_await(inner, span),
            A::Range { start, end } => ExprKind::Range {
                start: Box::new(self.lower_expr(start)),
                end: Box::new(self.lower_expr(end)),
            },
            A::Array(elems) if elems.iter().any(|e| matches!(e, ast::ArrayElem::Spread(_))) => {
                self.array_spread(elems, span)
            }
            A::Array(elems) => {
                let mut out = Vec::new();
                for el in elems {
                    if let ast::ArrayElem::Expr(x) = el {
                        out.push(self.lower_expr(x));
                    }
                }
                ExprKind::Array(out)
            }
            A::ArrayRepeat { value, count } => {
                let value = self.lower_expr(value);
                match self.const_len(count) {
                    Some(Ok(count)) => ExprKind::Repeat {
                        value: Box::new(value),
                        count,
                    },
                    Some(Err(count)) => ExprKind::RepeatN {
                        value: Box::new(value),
                        count,
                    },
                    None => ExprKind::Error,
                }
            }
            A::Comptime(inner) => return self.comptime(inner, span),
            A::Error => ExprKind::Error,
            other => {
                let what = match other {
                    A::Super => "`super` outside a method call",
                    A::Object { .. } => "object and struct literals",
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

    /// Decorators on a class that no `decorator function` handled (those are expanded and
    /// removed before resolution).
    fn unknown_decorators(&mut self, decs: &[ast::Decorator]) {
        for dec in decs {
            self.not_implemented(
                dec.span,
                "this decorator (no `decorator function` by that name)",
            );
        }
    }

    /// Whether `name` refers to a variable (which shadows a type of the same name).
    /// The unsafe intrinsic (spec §9.5) a call's callee names, unless user names shadow it:
    /// `ptr.of`, `unsafe.cast`, `sizeof` or `alignof`.
    fn intrinsic(&self, callee: &ast::Expr) -> Option<&'static str> {
        let free = |r: &Resolver, n: &str| {
            !r.is_value_name(n)
                && !r.top_fns.contains_key(&r.mods, n)
                && !r.top_structs.contains_key(&r.mods, n)
        };
        match &callee.kind {
            ast::ExprKind::Ident(n) if n == "sizeof" && free(self, "sizeof") => Some("sizeof"),
            ast::ExprKind::Ident(n) if n == "alignof" && free(self, "alignof") => Some("alignof"),
            ast::ExprKind::Member { object, prop, .. } => {
                match (&object.kind, prop.name.as_str()) {
                    (ast::ExprKind::Ident(o), "of") if o == "ptr" && free(self, "ptr") => {
                        Some("ptr.of")
                    }
                    (ast::ExprKind::Ident(o), "bytes") if o == "ptr" && free(self, "ptr") => {
                        Some("ptr.bytes")
                    }
                    (ast::ExprKind::Ident(o), "cast") if o == "unsafe" => Some("unsafe.cast"),
                    (ast::ExprKind::Ident(o), "readFile") if o == "comptime" => {
                        Some("comptime.readFile")
                    }
                    _ => None,
                }
            }
            ast::ExprKind::Ident(n) if n == "__raw_arena_alloc" => Some("__raw_arena_alloc"),
            ast::ExprKind::Ident(n) if n == "__throw_unchecked" => Some("__throw_unchecked"),
            ast::ExprKind::Ident(n) if n == "__exit_uncaught" => Some("__exit_uncaught"),
            ast::ExprKind::Ident(n) if n == "__worker_take" => Some("__worker_take"),
            _ => None,
        }
    }

    /// A field's `@json.name("key")` (spec §11.4); other decorators on fields aren't
    /// implemented.
    fn json_name(&mut self, decs: &[ast::Decorator]) -> Option<String> {
        let mut out = None;
        for dec in decs {
            let is_json =
                dec.path.len() == 2 && dec.path[0].name == "json" && dec.path[1].name == "name";
            if !is_json {
                self.not_implemented(dec.span, "decorators");
                continue;
            }
            match dec.args.as_deref() {
                Some([a]) if a.name.is_none() && !a.spread => match &a.value.kind {
                    ast::ExprKind::Lit(ast::Lit::Str(k)) => out = Some(k.clone()),
                    _ => self.error(
                        codes::MISMATCHED_TYPES,
                        a.value.span,
                        "`@json.name` takes a string literal",
                        "not a string literal",
                    ),
                },
                _ => self.error(
                    codes::WRONG_ARG_COUNT,
                    dec.span,
                    "`@json.name` takes one argument: the key",
                    "e.g. `@json.name(\"user_id\")`",
                ),
            }
        }
        out
    }

    /// Whether `callee` is `reflect.typeOf`, `reflect` being the core library's `__reflect`
    /// (as `std/reflect` exports it).
    fn reflect_call(&self, callee: &ast::Expr) -> bool {
        let ast::ExprKind::Member { object, prop, .. } = &callee.kind else {
            return false;
        };
        let ast::ExprKind::Ident(n) = &object.kind else {
            return false;
        };
        prop.name == "typeOf"
            && !self.is_value_name(n)
            && self.top_structs.get(&self.mods, n).is_some_and(|id| {
                self.top_structs
                    .prelude()
                    .any(|(pn, pid)| pn == "__reflect" && pid == id)
            })
    }

    fn is_value_name(&self, name: &str) -> bool {
        self.f.scopes.iter().any(|s| s.names.contains_key(name))
            || self.top_globals.contains_key(&self.mods, name)
    }

    /// The type `e` names (a type parameter, a primitive, a struct, class or enum), unless a
    /// variable hides it or the type has a static member `member`.
    fn type_named(&mut self, e: &ast::Expr, member: &str) -> Option<Ty> {
        let ast::ExprKind::Ident(n) = &e.kind else {
            return None;
        };
        if self.is_value_name(n) {
            return None;
        }
        if let Some((_, t)) = self.f.type_params.iter().rev().find(|(p, _)| p == n) {
            return Some(t.clone());
        }
        if let Some(t) = Ty::from_name(n) {
            return Some(t);
        }
        if let Some(&sid) = self.top_structs.get(&self.mods, n) {
            let def = &self.structs[sid.0 as usize];
            if def.static_field(member).is_some()
                || def.static_method(member).is_some()
                || !def.generics.is_empty()
            {
                return None;
            }
            return Some(self.struct_ty(sid));
        }
        if let Some(&id) = self.top_enums.get(&self.mods, n) {
            return Some(self.enum_ty(id));
        }
        None
    }

    /// A struct of the core library (`FieldInfo`) as a type.
    fn prelude_struct_ty(&self, name: &str) -> Ty {
        match self.top_structs.prelude().find(|(n, _)| *n == name) {
            Some((_, sid)) => self.struct_ty(*sid),
            None => Ty::Error,
        }
    }

    /// Whether `e` names a class, or a struct with static members (as in `Animal.count`,
    /// `Vec3.zero()`), not shadowed by a variable.
    fn names_class(&self, e: &ast::Expr) -> bool {
        matches!(&e.kind, ast::ExprKind::Ident(n)
        if !self.is_value_name(n)
            && self.top_structs.get(&self.mods, n).is_some_and(|id| {
                let d = &self.structs[id.0 as usize];
                d.is_class
                    || (!d.is_interface
                        && (!d.static_methods.is_empty() || !d.static_getters.is_empty()))
            }))
    }

    /// `Class.name`: a static field or static method (checking visibility).
    fn static_member(&mut self, class: &ast::Expr, prop: &ast::Ident) -> Option<ExprKind> {
        let ast::ExprKind::Ident(cname) = &class.kind else {
            return None;
        };
        let sid = *self.top_structs.get(&self.mods, cname).expect("checked");
        let def = &self.structs[sid.0 as usize];
        let getter = def
            .static_getters
            .iter()
            .find(|(n, _)| *n == prop.name)
            .map(|(_, f)| *f);
        let (field, method) = (def.static_field(&prop.name), def.static_method(&prop.name));
        // (For the language server: `Class` links to its declaration; the index links the
        // member, `Class.name`'s last part.)
        self.name_refs.push((class.span, def.name_span));
        let def = &self.structs[sid.0 as usize];
        let found = match (field, method) {
            (Some(g), _) => ExprKind::Var(Var::Global(g)),
            (_, Some(f)) => ExprKind::Fn(f),
            _ if getter.is_some() => {
                if !self.structs[sid.0 as usize].is_public(&prop.name) && self.f.owner != Some(sid)
                {
                    let decl = self.structs[sid.0 as usize].name_span;
                    self.private_access(cname, &prop.name, prop.span, decl);
                }
                let callee = self.expr(ExprKind::Fn(getter.expect("checked")), prop.span);
                return Some(ExprKind::Call {
                    callee: Box::new(callee),
                    args: Vec::new(),
                    type_args: Vec::new(),
                });
            }
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
                if self.top_enums.contains_key(&self.mods, n) && !self.is_value_name(n)),
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
        let eid = *self.top_enums.get(&self.mods, name)?;
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
            [e, v] => match self.top_enums.get(&self.mods, &e.name) {
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
        if let Some(id) = self.top_structs.get(&self.mods, name) {
            return Some(self.struct_ty(*id));
        }
        self.top_enums
            .get(&self.mods, name)
            .map(|id| self.enum_ty(*id))
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
                // A core-library type (`Map`) gives way to an enum variant of that name (#89).
                let core_type_shadowed = |r: &Self, n: &ast::Ident| {
                    r.top_structs.get(&r.mods, &n.name).is_some_and(|sid| {
                        let def = &r.structs[sid.0 as usize];
                        r.module_of(def.name_span.file) == PRELUDE_MODULE
                            && r.enums
                                .iter()
                                .any(|e| e.variants.iter().any(|v| v.name == n.name))
                    })
                };
                match path.as_slice() {
                    [n] if self.top_structs.contains_key(&self.mods, &n.name)
                        && !core_type_shadowed(self, n) =>
                    {
                        PatKind::Struct {
                            sid: *self.top_structs.get(&self.mods, &n.name).expect("checked"),
                            fields,
                        }
                    }
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
                match self.top_structs.get(&self.mods, &path[0].name) {
                    Some(id) if self.structs[id.0 as usize].is_class => {
                        self.error(
                            codes::UNKNOWN_OBJECT_TYPE,
                            path[0].span,
                            format!("`{}` is a class", path[0].name),
                            format!("create it with `new {}(…)`", path[0].name),
                        );
                        return ExprKind::Error;
                    }
                    Some(id) => {
                        let decl = self.structs[id.0 as usize].name_span;
                        self.name_refs.push((path[0].span, decl));
                        Some(*id)
                    }
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
                        key: InitKey::Name,
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
                        key: InitKey::Name,
                    });
                }
                ast::ObjField::KeyValue {
                    key: ast::PropKey::Str(k, s),
                    value,
                } => {
                    let value = self.lower_expr(value);
                    inits.push(FieldInit {
                        name: k.clone(),
                        name_span: *s,
                        value,
                        key: InitKey::Str,
                    });
                }
                ast::ObjField::Computed { key, value } => {
                    let k = self.lower_expr(key);
                    let value = self.lower_expr(value);
                    inits.push(FieldInit {
                        name: String::new(),
                        name_span: key.span,
                        value,
                        key: InitKey::Computed(k),
                    });
                }
                ast::ObjField::Spread(e) => {
                    let value = self.lower_expr(e);
                    inits.push(FieldInit {
                        name: String::new(),
                        name_span: e.span,
                        value,
                        key: InitKey::Spread,
                    });
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
    /// `reactive` fields (spec §16.1), with their annotated value type: their initializer
    /// becomes `__reactive<T>(init)`.
    reactive_fields: HashMap<String, Option<&'a ast::Type>>,
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
/// `type` as the type of a `comptime` parameter.
fn is_type_kind(t: &ast::Type) -> bool {
    matches!(&t.kind, ast::TypeKind::Path { path, args } if path.len() == 1 && path[0].name == "type" && args.is_empty())
}

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
        ast::PatternKind::Rest(Some(n)) => out.push(n),
        ast::PatternKind::Struct { fields, .. } => {
            for f in fields {
                match &f.pat {
                    Some(p) => pattern_idents(p, out),
                    None => out.push(&f.name),
                }
            }
        }
        _ => {}
    }
}

/// Parameters with destructuring patterns get hidden names (`<arg0>`), and the patterns to
/// destructure them with.
fn split_destructured(params: &[ast::Param]) -> (Vec<ast::Param>, Vec<(ast::Ident, ast::Pattern)>) {
    let mut out = Vec::new();
    let mut pats = Vec::new();
    for (i, p) in params.iter().enumerate() {
        match &p.pat.kind {
            ast::PatternKind::Ident(_) => out.push(p.clone()),
            _ => {
                let name = ast::Ident {
                    name: format!("<arg{i}>"),
                    span: p.pat.span,
                };
                pats.push((name.clone(), p.pat.clone()));
                out.push(ast::Param {
                    pat: ast::Pattern {
                        kind: ast::PatternKind::Ident(name),
                        span: p.pat.span,
                    },
                    ..p.clone()
                });
            }
        }
    }
    (out, pats)
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

// ---------------------------------------------------------------- async desugaring (spec §10.1)

/// Small AST builders for desugarings.
mod build {
    use pico_diag::Span;
    use pico_syntax::ast;

    pub fn e(kind: ast::ExprKind, span: Span) -> ast::Expr {
        ast::Expr { kind, span }
    }

    pub fn ident(name: &str, span: Span) -> ast::Expr {
        e(ast::ExprKind::Ident(name.to_string()), span)
    }

    pub fn member(object: ast::Expr, prop: &str, span: Span) -> ast::Expr {
        e(
            ast::ExprKind::Member {
                object: Box::new(object),
                prop: ast::Ident {
                    name: prop.to_string(),
                    span,
                },
                optional: false,
            },
            span,
        )
    }

    pub fn call(callee: ast::Expr, args: Vec<ast::Expr>, span: Span) -> ast::Expr {
        e(
            ast::ExprKind::Call {
                callee: Box::new(callee),
                type_args: Vec::new(),
                args: args
                    .into_iter()
                    .map(|value| ast::Arg {
                        name: None,
                        spread: false,
                        value,
                    })
                    .collect(),
            },
            span,
        )
    }

    pub fn stmt(expr: ast::Expr) -> ast::Stmt {
        let span = expr.span;
        ast::Stmt {
            kind: ast::StmtKind::Expr { expr, semi: true },
            span,
        }
    }

    pub fn tail(expr: ast::Expr) -> ast::Stmt {
        let span = expr.span;
        ast::Stmt {
            kind: ast::StmtKind::Expr { expr, semi: false },
            span,
        }
    }

    pub fn const_(name: &str, init: ast::Expr) -> ast::Stmt {
        let span = init.span;
        ast::Stmt {
            kind: ast::StmtKind::Let {
                kind: ast::BindingKind::Const,
                pat: ast::Pattern {
                    kind: ast::PatternKind::Ident(ast::Ident {
                        name: name.to_string(),
                        span,
                    }),
                    span,
                },
                ty: None,
                init: Some(init),
                comptime: false,
            },
            span,
        }
    }

    pub fn block(stmts: Vec<ast::Stmt>, span: Span) -> ast::Expr {
        e(ast::ExprKind::Block(ast::Block { stmts, span }), span)
    }

    pub fn named_type(name: &str, args: Vec<ast::Type>, span: Span) -> ast::Type {
        ast::Type {
            kind: ast::TypeKind::Path {
                path: vec![ast::Ident {
                    name: name.to_string(),
                    span,
                }],
                args,
            },
            span,
        }
    }
}

impl Resolver {
    /// `async function f(…): Promise<T> throws E { body }` →
    /// `function f(…): Promise<T, E> { const __p = new Promise<T, E>(); __p.__run(<step>); __p }`
    /// where `<step>` is a generator arrow over `body` (its `await`s yield, its `return`s
    /// resolve `__p`). Without an annotated `Promise<T>` result it's an error (`None`).
    fn desugar_async(&mut self, f: &ast::FnDecl) -> Option<ast::FnDecl> {
        let span = f.span;
        let elem = match &f.ret {
            Some(ast::Type {
                kind: ast::TypeKind::Path { path, args },
                ..
            }) if path.len() == 1 && path[0].name == "Promise" && args.len() == 1 => {
                args[0].clone()
            }
            _ => {
                self.error(
                    codes::MISSING_PARAM_TYPE,
                    f.name.as_ref().map_or(span, |n| n.span),
                    "an async function needs its `Promise<T>` result type",
                    "add `: Promise<T>`",
                );
                return None;
            }
        };
        // What it throws travels with the promise (several errors: `Error`).
        let err = f.throws.as_ref().map(|t| match &t.kind {
            ast::TypeKind::Union(_) => build::named_type("Error", Vec::new(), t.span),
            _ => t.clone(),
        });
        let mut targs = vec![elem.clone()];
        targs.extend(err);
        let promise_ty = build::named_type("Promise", targs.clone(), span);
        let is_void = matches!(&elem.kind, ast::TypeKind::Path { path, args }
            if args.is_empty() && path.len() == 1 && path[0].name == "void");
        // The step body: `body`, ending by resolving `__p` with its value.
        let mut stmts = match &f.body {
            Some(ast::FnBody::Block(b)) => b.stmts.clone(),
            Some(ast::FnBody::Expr(x)) => vec![build::tail((**x).clone())],
            None => Vec::new(),
        };
        let resolve = |v: ast::Expr| {
            build::call(
                build::member(build::ident("__p", span), "__hold", span),
                vec![v],
                span,
            )
        };
        match stmts.pop() {
            Some(ast::Stmt {
                kind: ast::StmtKind::Expr { expr, semi: false },
                ..
            }) if !is_void
                && !matches!(
                    expr.kind,
                    ast::ExprKind::Return(_) | ast::ExprKind::Throw(_)
                ) =>
            {
                stmts.push(build::stmt(resolve(expr)))
            }
            Some(last) => stmts.push(last),
            None => {}
        }
        if is_void {
            stmts.push(build::stmt(resolve(build::call(
                build::ident("__void", span),
                Vec::new(),
                span,
            ))));
        }
        // (Named for messages: `<async f>`.)
        let shown = f
            .name
            .as_ref()
            .map_or("function".to_string(), |n| n.name.clone());
        let step = ast::FnDecl {
            name: Some(ast::Ident {
                name: format!("<async {shown}>"),
                span: f.name.as_ref().map_or(span, |n| n.span),
            }),
            flags: ast::FnFlags {
                arrow: true,
                generator: true,
                is_async: true,
                ..Default::default()
            },
            generics: Vec::new(),
            params: Vec::new(),
            ret: None,
            throws: None,
            where_clause: Vec::new(),
            body: Some(ast::FnBody::Block(ast::Block { stmts, span })),
            span,
        };
        let new_promise = build::e(
            ast::ExprKind::New {
                callee: Box::new(build::ident("Promise", span)),
                type_args: targs,
                args: Vec::new(),
            },
            span,
        );
        let run = build::call(
            build::member(build::ident("__p", span), "__run", span),
            vec![build::e(ast::ExprKind::Function(Box::new(step)), span)],
            span,
        );
        let body = ast::Block {
            stmts: vec![
                build::const_("__p", new_promise),
                build::stmt(run),
                build::tail(build::ident("__p", span)),
            ],
            span,
        };
        Some(ast::FnDecl {
            flags: ast::FnFlags {
                is_async: false,
                ..f.flags
            },
            ret: Some(promise_ty),
            throws: None,
            body: Some(ast::FnBody::Block(body)),
            ..f.clone()
        })
    }

    /// `async function* g(…): AsyncIterator<T> throws E { body }` →
    /// `function g(…): AsyncIterator<T, E> { const __it = new AsyncIterator<T, E>();
    /// __it.__start(<step>); __it }`; the step's `yield v` stores `v` in `__it` and pauses.
    fn desugar_async_gen(&mut self, f: &ast::FnDecl) -> Option<ast::FnDecl> {
        let span = f.span;
        let elem = match &f.ret {
            Some(ast::Type {
                kind: ast::TypeKind::Path { path, args },
                ..
            }) if path.len() == 1 && path[0].name == "AsyncIterator" && args.len() == 1 => {
                args[0].clone()
            }
            _ => {
                self.error(
                    codes::MISSING_PARAM_TYPE,
                    f.name.as_ref().map_or(span, |n| n.span),
                    "an async generator needs its `AsyncIterator<T>` result type",
                    "add `: AsyncIterator<T>`",
                );
                return None;
            }
        };
        let err = f.throws.as_ref().map(|t| match &t.kind {
            ast::TypeKind::Union(_) => build::named_type("Error", Vec::new(), t.span),
            _ => t.clone(),
        });
        let mut targs = vec![elem];
        targs.extend(err);
        let mut stmts = match &f.body {
            Some(ast::FnBody::Block(b)) => b.stmts.clone(),
            _ => Vec::new(),
        };
        // (Its last expression's value is dropped.)
        if let Some(ast::Stmt {
            kind: ast::StmtKind::Expr { semi, .. },
            ..
        }) = stmts.last_mut()
        {
            *semi = true;
        }
        let shown = f
            .name
            .as_ref()
            .map_or("function".to_string(), |n| n.name.clone());
        let step = ast::FnDecl {
            name: Some(ast::Ident {
                name: format!("<async* {shown}>"),
                span: f.name.as_ref().map_or(span, |n| n.span),
            }),
            flags: ast::FnFlags {
                arrow: true,
                generator: true,
                is_async: true,
                ..Default::default()
            },
            generics: Vec::new(),
            params: Vec::new(),
            ret: None,
            throws: None,
            where_clause: Vec::new(),
            body: Some(ast::FnBody::Block(ast::Block { stmts, span })),
            span,
        };
        let new_it = build::e(
            ast::ExprKind::New {
                callee: Box::new(build::ident("AsyncIterator", span)),
                type_args: targs.clone(),
                args: Vec::new(),
            },
            span,
        );
        let start = build::call(
            build::member(build::ident("__it", span), "__start", span),
            vec![build::e(ast::ExprKind::Function(Box::new(step)), span)],
            span,
        );
        let body = ast::Block {
            stmts: vec![
                build::const_("__it", new_it),
                build::stmt(start),
                build::tail(build::ident("__it", span)),
            ],
            span,
        };
        Some(ast::FnDecl {
            flags: ast::FnFlags {
                is_async: false,
                generator: false,
                ..f.flags
            },
            ret: Some(build::named_type("AsyncIterator", targs, span)),
            throws: None,
            body: Some(ast::FnBody::Block(body)),
            ..f.clone()
        })
    }

    /// `for await (pat of iter) body` →
    /// `{ const it = iter.asyncIterator(); loop { const n = await it.next(); if (n == null) break;
    /// const pat = n; body } }`.
    fn for_await(
        &mut self,
        kind: ast::BindingKind,
        pat: &ast::Pattern,
        iter: &ast::Expr,
        body: &ast::Expr,
        span: Span,
    ) -> Expr {
        self.awaits += 1;
        let it = format!("__forAwait{}", self.awaits);
        let n = format!("__next{}", self.awaits);
        let next = build::e(
            ast::ExprKind::Await(Box::new(build::call(
                build::member(build::ident(&it, span), "next", span),
                Vec::new(),
                span,
            ))),
            span,
        );
        let is_null = build::e(
            ast::ExprKind::Binary {
                op: ast::BinOp::Eq,
                lhs: Box::new(build::ident(&n, span)),
                rhs: Box::new(build::e(ast::ExprKind::Lit(ast::Lit::Null), span)),
            },
            span,
        );
        let stop = build::e(
            ast::ExprKind::If {
                cond: Box::new(is_null),
                then: Box::new(build::e(ast::ExprKind::Break(None), span)),
                else_: None,
            },
            span,
        );
        let bind = ast::Stmt {
            kind: ast::StmtKind::Let {
                kind,
                pat: pat.clone(),
                ty: None,
                init: Some(build::ident(&n, span)),
                comptime: false,
            },
            span: pat.span,
        };
        let lp = build::e(
            ast::ExprKind::Loop(ast::Block {
                stmts: vec![
                    build::const_(&n, next),
                    build::stmt(stop),
                    bind,
                    build::stmt(body.clone()),
                ],
                span,
            }),
            span,
        );
        let source = build::call(
            build::member(iter.clone(), "asyncIterator", span),
            Vec::new(),
            span,
        );
        let b = build::block(vec![build::const_(&it, source), build::stmt(lp)], span);
        self.lower_expr(&b)
    }

    /// An inline worker's function (spec §10.4) must not capture anything: E0346 when a
    /// lambda written there does.
    fn check_capture_free(&mut self, f: &Expr) {
        // The entry module's variables aren't initialized in a worker either.
        fn entry_globals(e: &Expr, r: &Resolver, out: &mut Vec<String>) {
            let g = match &e.kind {
                ExprKind::Var(Var::Global(g)) => Some(*g),
                ExprKind::Assign {
                    target: Target::Var(Var::Global(g)),
                    ..
                } => Some(*g),
                _ => None,
            };
            if let Some(g) = g {
                let b = &r.globals[g.0 as usize];
                if b.span.file == r.file && !out.contains(&b.name) {
                    out.push(b.name.clone());
                }
            }
            e.for_each_child(&mut |c| entry_globals(c, r, out));
        }
        if let ExprKind::Closure(id) = &f.kind
            && let Some(Some(func)) = self.fns.get(id.0 as usize)
            && let Some(c) = func.closure.as_ref()
        {
            let mut names: Vec<String> = c
                .captures
                .iter()
                .map(|k| func.locals[k.inner.0 as usize].name.clone())
                .collect();
            for st in &func.body.stmts {
                match st {
                    Stmt::Let { init: Some(e), .. } | Stmt::Expr(e) => {
                        entry_globals(e, self, &mut names)
                    }
                    Stmt::Let { .. } => {}
                }
            }
            if let Some(t) = &func.body.tail {
                entry_globals(t, self, &mut names);
            }
            if names.is_empty() {
                return;
            }
            self.error(
                codes::WORKER_CAPTURES,
                f.span,
                format!(
                    "an inline worker's function can't capture anything (it captures `{}`)",
                    names.join("`, `")
                ),
                "runs in its own thread, with its own heap",
            );
        }
    }

    /// `await x`: inside an async function's step `{ const a = x; yield a.__suspend();
    /// a.__get() }`; at the entry's top level `__awaitTop(x)`.
    fn lower_await(&mut self, inner: &ast::Expr, span: Span) -> ExprKind {
        if self.f.async_step {
            self.awaits += 1;
            let name = format!("__await{}", self.awaits);
            let suspend = build::call(
                build::member(build::ident(&name, span), "__suspend", span),
                Vec::new(),
                span,
            );
            let get = build::call(
                build::member(build::ident(&name, span), "__get", span),
                Vec::new(),
                span,
            );
            // `{ const a = x; <yield a.__suspend()>; a.__get() }`, the `yield` built directly
            // (an async generator would turn a source `yield` into a value).
            self.f.scopes.push(Scope::default());
            let mut stmts = Vec::new();
            self.lower_stmt(&build::const_(&name, inner.clone()), &mut stmts, None);
            let suspend = self.lower_expr(&suspend);
            stmts.push(Stmt::Expr(
                self.expr(ExprKind::Yield(Box::new(suspend)), span),
            ));
            let get = self.lower_expr(&get);
            self.f.scopes.pop();
            return ExprKind::Block(Block {
                stmts,
                tail: Some(Box::new(get)),
                span,
            });
        }
        if self.f.in_entry && !self.f.is_closure {
            let c = build::call(build::ident("__awaitTop", span), vec![inner.clone()], span);
            return self.lower_expr(&c).kind;
        }
        self.error(
            codes::AWAIT_OUTSIDE_ASYNC,
            span,
            "`await` outside an async function",
            "not inside an `async function`",
        );
        ExprKind::Error
    }
}

/// `Reactive<t>`.
fn reactive_type(t: &ast::Type) -> ast::Type {
    ast::Type {
        kind: ast::TypeKind::Path {
            path: vec![ast::Ident {
                name: "Reactive".into(),
                span: t.span,
            }],
            args: vec![t.clone()],
        },
        span: t.span,
    }
}

/// `__reactive<t>(init)` (or `__reactive(init)`): a new reactive cell (spec §16.1).
fn reactive_cell(init: &ast::Expr, t: Option<&ast::Type>) -> ast::Expr {
    ast::Expr {
        kind: ast::ExprKind::Call {
            callee: Box::new(ast::Expr {
                kind: ast::ExprKind::Ident("__reactive".into()),
                span: init.span,
            }),
            type_args: t.into_iter().cloned().collect(),
            args: vec![ast::Arg {
                name: None,
                spread: false,
                value: init.clone(),
            }],
        },
        span: init.span,
    }
}
