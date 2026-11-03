//! Decorators (spec §11.2): `decorator function`s run at compile time on descriptions of the
//! declarations they decorate (`std/meta`), before those are compiled. What they ask for
//! (members, hidden statics, wrapped bodies, diagnostics) is applied to the syntax tree.
//!
//! The decorators run in a program of their own: the core library, `std/meta`, the
//! declarations the decorator functions refer to (by name, transitively) and, in each
//! decorator's module, one `comptime { decorator(new FunctionDecl(…), args…) }` per use.
//! Generated code lives in files of its own, mapped to the module it belongs to
//! (`Modules::file_modules`).

use ppl_diag::{Diagnostic, FileId, Severity, SourceMap, Span, codes};
use ppl_sema::Modules;
use ppl_syntax::TokenKind;
use ppl_syntax::ast::{self, DeclKind, MemberKind, StmtKind};
use std::collections::{HashMap, HashSet};

/// The outcome of running a program's decorators.
#[derive(Default)]
pub struct Expansion {
    pub diags: Vec<Diagnostic>,
    /// What the decorators printed (part of the compile-time log).
    pub log: String,
    /// Each file of generated code: the decorator use it came from, and what it is.
    pub origins: HashMap<FileId, (Span, String)>,
}

impl Expansion {
    /// Points problems found in generated code at the decorator use that generated it.
    pub fn annotate(&self, diags: &mut [Diagnostic]) {
        for d in diags {
            let Some((at, what)) = d.main_span().and_then(|s| self.origins.get(&s.file)) else {
                continue;
            };
            *d = d.clone().secondary(*at, format!("in the {what}"));
        }
    }
}

/// A use of a decorator on a declaration.
struct Use {
    id: usize,
    /// The decorated statement.
    stmt: usize,
    /// The decorator: its module and its name there.
    decorator: (FileId, String),
    span: Span,
    /// The source text of the decorator's arguments.
    args: Vec<String>,
    /// For each argument that is a bare name, the name.
    bare: Vec<Option<String>>,
}

/// Runs the decorators of `stmts` (the whole program, prelude first) and applies what they
/// ask for. Their uses are removed, and so are decorator functions, unless `keep_functions`
/// (the language server's analysis: the program's own decorator functions stay, as ordinary
/// functions, so their code has hovers and links).
pub fn expand(
    sm: &mut SourceMap,
    stmts: &mut Vec<ast::Stmt>,
    modules: &mut Modules,
    entry: FileId,
    cfg: &ppl_interp::comptime::Config,
    keep_functions: bool,
) -> Expansion {
    let mut out = Expansion::default();
    // Decorator functions, by (module, name): the type their first parameter takes.
    let mut decorators: HashMap<(FileId, String), String> = HashMap::new();
    for st in stmts.iter() {
        if let StmtKind::Decl(d) = &st.kind
            && let DeclKind::Function(f) = &d.kind
            && f.flags.decorator
            && let Some(name) = &f.name
        {
            let first = f
                .params
                .first()
                .and_then(|p| p.ty.as_ref())
                .map(|t| text(sm, t.span))
                .unwrap_or_default();
            decorators.insert((st.span.file, name.name.clone()), first);
        }
    }
    if decorators.is_empty() {
        return out;
    }
    // The uses.
    let mut uses: Vec<Use> = Vec::new();
    for (i, st) in stmts.iter().enumerate() {
        let StmtKind::Decl(d) = &st.kind else {
            continue;
        };
        for dec in &d.decorators {
            let [name] = &dec.path[..] else { continue };
            let Some(target) = find_decorator(&decorators, modules, st.span.file, &name.name)
            else {
                continue;
            };
            let what = match &d.kind {
                DeclKind::Function(_) => "FunctionDecl",
                DeclKind::Class(_) | DeclKind::Struct(_) => "ClassDecl",
                _ => {
                    out.diags.push(
                        Diagnostic::error(
                            codes::NOT_IMPLEMENTED,
                            "decorators on this kind of declaration aren't implemented yet",
                        )
                        .primary(dec.span, "only classes, structs and functions for now"),
                    );
                    continue;
                }
            };
            let takes = decorators[&target].trim_start_matches("__");
            if (takes == "FunctionDecl" || takes == "ClassDecl") && takes != what {
                let kind = if what == "FunctionDecl" {
                    "functions"
                } else {
                    "classes and structs"
                };
                out.diags.push(
                    Diagnostic::error(
                        codes::DECORATOR,
                        format!("`@{}` decorates a `{takes}`, not {kind}", name.name),
                    )
                    .primary(dec.span, "used on the wrong kind of declaration"),
                );
                continue;
            }
            let args = dec
                .args
                .iter()
                .flatten()
                .map(|a| text(sm, a.value.span))
                .collect();
            let bare = dec
                .args
                .iter()
                .flatten()
                .map(|a| match &a.value.kind {
                    ast::ExprKind::Ident(n) => Some(n.clone()),
                    _ => None,
                })
                .collect();
            uses.push(Use {
                bare,
                id: uses.len(),
                stmt: i,
                decorator: target,
                span: dec.span,
                args,
            });
        }
    }
    if out.diags.iter().any(Diagnostic::is_error) {
        return out;
    }
    if !uses.is_empty() {
        let edits = match run_decorators(sm, stmts, modules, entry, cfg, &decorators, &uses) {
            Ok((edits, log)) => {
                out.log = log;
                edits
            }
            Err(d) => {
                out.diags = d;
                return out;
            }
        };
        apply(
            sm,
            stmts,
            modules,
            &uses,
            &edits,
            &mut out.diags,
            &mut out.origins,
        );
    }
    // Decorators are gone from the program that runs.
    let user = |file: FileId, d: &ast::Decorator| matches!(&d.path[..], [n] if find_decorator(&decorators, modules, file, &n.name).is_some());
    let mut kept = Vec::with_capacity(stmts.len());
    let mut type_decorators = Vec::new();
    for mut st in std::mem::take(stmts) {
        if let StmtKind::Decl(d) = &mut st.kind {
            if let DeclKind::Function(f) = &mut d.kind
                && f.flags.decorator
            {
                if keep_functions && !modules.prelude.contains(&st.span.file) {
                    f.flags.decorator = false;
                    kept.push(st);
                }
                continue;
            }
            let file = st.span.file;
            // Reflection still sees them (spec §11.3).
            let name_span = match &d.kind {
                DeclKind::Class(c) => Some(c.name.span),
                DeclKind::Struct(s) => Some(s.name.span),
                _ => None,
            };
            if let Some(ns) = name_span {
                let text = |sp: Span| {
                    sm.file(sp.file).text[sp.start as usize..sp.end as usize].to_string()
                };
                let seen: Vec<(String, Vec<String>)> = d
                    .decorators
                    .iter()
                    .filter(|dec| user(file, dec))
                    .map(|dec| {
                        let name: Vec<&str> = dec.path.iter().map(|p| p.name.as_str()).collect();
                        let args = dec
                            .args
                            .iter()
                            .flatten()
                            .map(|a| match &a.name {
                                Some(n) => format!("{}: {}", n.name, text(a.value.span)),
                                None => text(a.value.span),
                            })
                            .collect();
                        (name.join("."), args)
                    })
                    .collect();
                if !seen.is_empty() {
                    type_decorators.push((ns, seen));
                }
            }
            d.decorators.retain(|dec| !user(file, dec));
        }
        kept.push(st);
    }
    *stmts = kept;
    modules.type_decorators.extend(type_decorators);
    out
}

/// The decorator `name` means in module `file`: declared there, or imported.
fn find_decorator(
    decorators: &HashMap<(FileId, String), String>,
    modules: &Modules,
    file: FileId,
    name: &str,
) -> Option<(FileId, String)> {
    let file = modules.file_modules.get(&file).copied().unwrap_or(file);
    if decorators.contains_key(&(file, name.to_string())) {
        return Some((file, name.to_string()));
    }
    let imported = modules
        .imports
        .iter()
        .find(|i| i.module == file && i.alias.name == name && i.name.is_some())
        .and_then(|imp| {
            let exported = &imp.name.as_ref()?.name;
            let local = modules.exports.get(&imp.target)?.get(exported)?;
            Some((imp.target, local.clone()))
        });
    if let Some(key) = imported {
        return decorators.contains_key(&key).then_some(key);
    }
    // The core library's decorators (`@derive`) are seen everywhere.
    decorators
        .keys()
        .find(|(f, n)| n == name && modules.prelude.contains(f))
        .cloned()
}

fn text(sm: &SourceMap, s: Span) -> String {
    sm.file(s.file).text[s.start as usize..s.end as usize].to_string()
}

fn quote(s: &str) -> String {
    format!("{s:?}")
}

fn str_list(xs: &[String]) -> String {
    let parts: Vec<String> = xs.iter().map(|x| quote(x)).collect();
    format!("[{}]", parts.join(", "))
}

/// `new ClassDecl(…)` / `new FunctionDecl(…)` for a use, with the class named as the
/// decorator's parameter names it.
fn descriptor(sm: &SourceMap, d: &ast::Decl, id: usize, class: &str) -> String {
    match &d.kind {
        DeclKind::Function(f) => {
            let (mut names, mut types) = (Vec::new(), Vec::new());
            for p in &f.params {
                names.push(match &p.pat.kind {
                    ast::PatternKind::Ident(n) => n.name.clone(),
                    _ => text(sm, p.pat.span),
                });
                types.push(p.ty.as_ref().map(|t| text(sm, t.span)).unwrap_or_default());
            }
            let ret = f
                .ret
                .as_ref()
                .map(|t| text(sm, t.span))
                .unwrap_or_else(|| "void".into());
            let name = f.name.as_ref().map(|n| n.name.clone()).unwrap_or_default();
            format!(
                "new {class}({id}, {}, {}, {}, {}, {})",
                quote(&name),
                str_list(&names),
                str_list(&types),
                quote(&ret),
                f.flags.is_async
            )
        }
        DeclKind::Class(ast::ClassDecl { name, members, .. })
        | DeclKind::Struct(ast::StructDecl { name, members, .. }) => {
            let kind = if matches!(d.kind, DeclKind::Class(_)) {
                "class"
            } else {
                "struct"
            };
            let (mut fields, mut types, mut methods) = (Vec::new(), Vec::new(), Vec::new());
            for m in members.iter().filter(|m| !m.mods.is_static) {
                match &m.kind {
                    MemberKind::Field { name, ty, .. } => {
                        fields.push(name.name.clone());
                        types.push(ty.as_ref().map(|t| text(sm, t.span)).unwrap_or_default());
                    }
                    // Parameter properties: `constructor(public id: i64)`.
                    MemberKind::Constructor(c) => {
                        for p in c.params.iter().filter(|p| !p.mods.is_empty()) {
                            if let ast::PatternKind::Ident(n) = &p.pat.kind {
                                fields.push(n.name.clone());
                                types.push(
                                    p.ty.as_ref().map(|t| text(sm, t.span)).unwrap_or_default(),
                                );
                            }
                        }
                    }
                    MemberKind::Method(f) => {
                        if let Some(n) = &f.name {
                            methods.push(n.name.clone());
                        }
                    }
                    _ => {}
                }
            }
            let generics = match &d.kind {
                DeclKind::Class(c) => &c.generics,
                DeclKind::Struct(s) => &s.generics,
                _ => unreachable!("checked"),
            };
            let params: Vec<String> = generics.iter().map(|g| g.name.name.clone()).collect();
            format!(
                "new {class}({id}, {}, {}, {}, {}, {}, {})",
                quote(&name.name),
                quote(kind),
                str_list(&fields),
                str_list(&types),
                str_list(&methods),
                str_list(&params)
            )
        }
        _ => unreachable!("checked"),
    }
}

/// The names a declaration's source text uses.
fn idents(sm: &SourceMap, s: Span) -> Vec<String> {
    let (tokens, _) = ppl_syntax::lex(s.file, &text(sm, s));
    tokens
        .into_iter()
        .filter_map(|t| match t.kind {
            TokenKind::Ident(n) => Some(n),
            _ => None,
        })
        .collect()
}

/// The names a top-level statement declares.
fn declares(st: &ast::Stmt) -> Vec<String> {
    match &st.kind {
        StmtKind::Decl(d) => match &d.kind {
            DeclKind::Function(f) => f.name.iter().map(|n| n.name.clone()).collect(),
            DeclKind::Struct(s) => vec![s.name.name.clone()],
            DeclKind::Class(c) => vec![c.name.name.clone()],
            DeclKind::Enum(e) => vec![e.name.name.clone()],
            DeclKind::Interface(i) => vec![i.name.name.clone()],
            DeclKind::TypeAlias { name, .. } => vec![name.name.clone()],
            _ => Vec::new(),
        },
        StmtKind::Let { pat, .. } => match &pat.kind {
            ast::PatternKind::Ident(n) => vec![n.name.clone()],
            _ => Vec::new(),
        },
        _ => Vec::new(),
    }
}

/// Compiles and runs the decorators; returns their requests and what they printed.
#[allow(clippy::too_many_arguments)]
fn run_decorators(
    sm: &mut SourceMap,
    stmts: &[ast::Stmt],
    modules: &Modules,
    entry: FileId,
    cfg: &ppl_interp::comptime::Config,
    decorators: &HashMap<(FileId, String), String>,
    uses: &[Use],
) -> Result<(Vec<ppl_interp::comptime::MetaEdit>, String), Vec<Diagnostic>> {
    let meta_file = modules
        .names
        .iter()
        .find(|(_, n)| n.as_str() == "std/meta")
        .map(|(f, _)| *f);
    let decorated: HashSet<usize> = uses.iter().map(|u| u.stmt).collect();
    // What the decorator functions refer to, transitively (by name: an over-approximation).
    // (Per module: a name used there means its own declaration, or what it imports by
    // that name.)
    let module_of = |f: FileId| modules.file_modules.get(&f).copied().unwrap_or(f);
    let declared = |m: FileId, n: &str| {
        stmts
            .iter()
            .any(|st| module_of(st.span.file) == m && declares(st).iter().any(|d| d == n))
            || modules.prelude.contains(&m)
                && stmts.iter().any(|st| {
                    modules.prelude.contains(&st.span.file) && declares(st).iter().any(|d| d == n)
                })
    };
    let mut keep: HashSet<usize> = HashSet::new();
    let mut seen: HashSet<(FileId, String)> = HashSet::new();
    let mut work: Vec<(FileId, String)> = Vec::new();
    let library = |f: FileId| modules.prelude.contains(&f) || Some(f) == meta_file;
    for (i, st) in stmts.iter().enumerate() {
        if let StmtKind::Decl(d) = &st.kind
            && matches!(&d.kind, DeclKind::Function(f) if f.flags.decorator)
            && !library(st.span.file)
        {
            keep.insert(i);
            let m = module_of(st.span.file);
            work.extend(idents(sm, st.span).into_iter().map(|n| (m, n)));
        }
    }
    while let Some((m, n)) = work.pop() {
        if !seen.insert((m, n.clone())) {
            continue;
        }
        // Imported: look in the module it comes from, under its name there.
        let (m, n) = match modules
            .imports
            .iter()
            .find(|i| i.module == m && i.alias.name == n && i.name.is_some())
        {
            Some(imp) => {
                let exported = &imp.name.as_ref().expect("checked").name;
                let local = modules
                    .exports
                    .get(&imp.target)
                    .and_then(|e| e.get(exported))
                    .cloned()
                    .unwrap_or_else(|| exported.clone());
                (imp.target, local)
            }
            None => (m, n),
        };
        for (i, st) in stmts.iter().enumerate() {
            if library(st.span.file) || decorated.contains(&i) || keep.contains(&i) {
                continue;
            }
            if module_of(st.span.file) == m && declares(st).contains(&n) {
                keep.insert(i);
                work.extend(idents(sm, st.span).into_iter().map(|n| (m, n)));
            }
        }
    }
    let mut program: Vec<ast::Stmt> = Vec::new();
    for (i, st) in stmts.iter().enumerate() {
        let library = modules.prelude.contains(&st.span.file) || Some(st.span.file) == meta_file;
        if !(library || keep.contains(&i)) {
            continue;
        }
        let mut st = st.clone();
        if let StmtKind::Decl(d) = &mut st.kind {
            let file = st.span.file;
            d.decorators.retain(|dec| {
                !matches!(&dec.path[..], [n] if find_decorator(decorators, modules, file, &n.name).is_some())
            });
            if let DeclKind::Function(f) = &mut d.kind {
                f.flags.decorator = false;
            }
        }
        program.push(st);
    }
    // In each decorator's module: run its uses.
    let mut modules = modules.clone();
    let mut by_module: HashMap<FileId, String> = HashMap::new();
    for u in uses {
        let StmtKind::Decl(d) = &stmts[u.stmt].kind else {
            unreachable!("a declaration")
        };
        let class = &decorators[&u.decorator];
        let mut args = vec![descriptor(sm, d, u.id, class)];
        // A bare name the decorator's module doesn't declare is passed as a string
        // (`@derive(Eq)`).
        for (a, bare) in u.args.iter().zip(&u.bare) {
            match bare {
                Some(n) if !declared(u.decorator.0, n) => args.push(quote(n)),
                _ => args.push(a.clone()),
            }
        }
        let code = by_module.entry(u.decorator.0).or_default();
        code.push_str(&format!(
            "comptime {{ {}({}); }}\n",
            u.decorator.1,
            args.join(", ")
        ));
    }
    let mut diags = Vec::new();
    let mut files: Vec<(FileId, String)> = by_module.into_iter().collect();
    files.sort_by_key(|(f, _)| f.0);
    for (module, code) in files {
        let name = format!("<decorators of {}>", sm.file(module).name);
        let f = sm.add(name, code.clone());
        modules.file_modules.insert(f, module);
        let (m, mut d) = ppl_syntax::parse_module(f, &code);
        diags.append(&mut d);
        program.extend(m.stmts);
    }
    if diags.iter().any(Diagnostic::is_error) {
        return Err(diags);
    }
    let span = Span::new(entry, 0, 0);
    let module = ast::Module {
        stmts: program,
        span,
    };
    let (hir, mut d) = ppl_sema::resolve_modules(&module, entry, &modules);
    diags.append(&mut d);
    if diags.iter().any(Diagnostic::is_error) {
        return Err(diags);
    }
    let (types, mut d) = ppl_sema::check(&hir);
    diags.append(&mut d);
    if diags.iter().any(Diagnostic::is_error) {
        return Err(diags);
    }
    let (mut mir, mut d) = ppl_mir::lower(&hir, &types, sm.file(entry), entry);
    diags.append(&mut d);
    if diags.iter().any(Diagnostic::is_error) {
        return Err(diags);
    }
    let (mut d, log, edits) = ppl_interp::comptime::run_meta(&mut mir, cfg);
    diags.append(&mut d);
    if diags.iter().any(Diagnostic::is_error) {
        return Err(diags);
    }
    Ok((edits, log))
}

/// Applies the decorators' requests to the program.
fn apply(
    sm: &mut SourceMap,
    stmts: &mut Vec<ast::Stmt>,
    modules: &mut Modules,
    uses: &[Use],
    edits: &[ppl_interp::comptime::MetaEdit],
    diags: &mut Vec<Diagnostic>,
    origins: &mut HashMap<FileId, (Span, String)>,
) {
    // Generated code: a file of its own in the declaration's module.
    let mut generated = |sm: &mut SourceMap, module: FileId, what: &str, code: String, at: Span| {
        let f = sm.add(format!("<{what}>"), code.clone());
        modules.file_modules.insert(f, module);
        origins.insert(f, (at, what.to_string()));
        let (m, d) = ppl_syntax::parse_module(f, &code);
        (m, d)
    };
    let mut statics: Vec<(FileId, ast::Stmt)> = Vec::new();
    for e in edits {
        let Some(u) = usize::try_from(e.decl).ok().and_then(|i| uses.get(i)) else {
            continue;
        };
        let module = stmts[u.stmt].span.file;
        let dec_name = &u.decorator.1;
        match e.kind.as_str() {
            "error" => diags.push(
                Diagnostic::error(codes::DECORATOR, e.a.clone())
                    .primary(u.span, format!("reported by `@{dec_name}`")),
            ),
            "warn" | "note" => {
                let sev = if e.kind == "warn" {
                    Severity::Warning
                } else {
                    Severity::Note
                };
                diags.push(
                    Diagnostic::new(sev, e.a.clone())
                        .primary(u.span, format!("reported by `@{dec_name}`")),
                );
            }
            "static" => {
                let code = format!("const {} = {}\n", e.a, e.b);
                let (m, mut d) = generated(
                    sm,
                    module,
                    &format!("static {} of @{dec_name}", e.a),
                    code,
                    u.span,
                );
                diags.append(&mut d);
                statics.extend(m.stmts.into_iter().map(|s| (module, s)));
            }
            "member" => {
                let StmtKind::Decl(d) = &mut stmts[u.stmt].kind else {
                    continue;
                };
                let (kw, members) = match &mut d.kind {
                    DeclKind::Class(c) => ("class", &mut c.members),
                    DeclKind::Struct(s) => ("struct", &mut s.members),
                    _ => continue,
                };
                let code = format!("{kw} __Generated {{\n{}\n}}\n", e.a);
                let (m, mut ds) = generated(
                    sm,
                    module,
                    &format!("member added by @{dec_name}"),
                    code,
                    u.span,
                );
                diags.append(&mut ds);
                for st in m.stmts {
                    if let StmtKind::Decl(ast::Decl {
                        kind:
                            DeclKind::Class(ast::ClassDecl { members: ms, .. })
                            | DeclKind::Struct(ast::StructDecl { members: ms, .. }),
                        ..
                    }) = st.kind
                    {
                        // A struct's members are all public: `public` (as the AST builder
                        // writes it) says nothing there.
                        members.extend(ms.into_iter().map(|mut m| {
                            if kw == "struct" && m.mods.visibility == Some(ast::Visibility::Public)
                            {
                                m.mods.visibility = None;
                            }
                            m
                        }));
                    }
                }
            }
            "wrap" => {
                let StmtKind::Decl(d) = &mut stmts[u.stmt].kind else {
                    continue;
                };
                let DeclKind::Function(f) = &mut d.kind else {
                    continue;
                };
                let code = format!("function __wrapped() {{\n{}\n}}\n", e.a);
                let (m, mut ds) = generated(
                    sm,
                    module,
                    &format!("body made by @{dec_name}"),
                    code,
                    u.span,
                );
                diags.append(&mut ds);
                let Some(body) = m.stmts.into_iter().find_map(|st| match st.kind {
                    StmtKind::Decl(ast::Decl {
                        kind: DeclKind::Function(w),
                        ..
                    }) => w.body,
                    _ => None,
                }) else {
                    continue;
                };
                let ast::FnBody::Block(mut block) = body else {
                    continue;
                };
                // `const __orig = (params): R => { the original body }`, then the new body.
                let span = f.span;
                let orig = ast::FnDecl {
                    name: None,
                    flags: ast::FnFlags {
                        arrow: true,
                        ..Default::default()
                    },
                    generics: Vec::new(),
                    params: f.params.clone(),
                    ret: f.ret.clone(),
                    throws: f.throws.clone(),
                    where_clause: Vec::new(),
                    body: f.body.take(),
                    span,
                };
                let binding = ast::Stmt {
                    kind: StmtKind::Let {
                        kind: ast::BindingKind::Const,
                        pat: ast::Pattern {
                            kind: ast::PatternKind::Ident(ast::Ident {
                                name: "__orig".into(),
                                span,
                            }),
                            span,
                        },
                        ty: None,
                        init: Some(ast::Expr {
                            kind: ast::ExprKind::Function(Box::new(orig)),
                            span,
                        }),
                        comptime: false,
                    },
                    span,
                };
                block.stmts.insert(0, binding);
                f.body = Some(ast::FnBody::Block(block));
            }
            _ => {}
        }
    }
    // Hidden statics come first in their module, so they're initialized before its code runs.
    for (module, st) in statics.into_iter().rev() {
        let at = stmts
            .iter()
            .position(|s| {
                modules
                    .file_modules
                    .get(&s.span.file)
                    .copied()
                    .unwrap_or(s.span.file)
                    == module
            })
            .unwrap_or(stmts.len());
        stmts.insert(at, st);
    }
}
