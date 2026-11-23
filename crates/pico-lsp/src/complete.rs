//! Completion (PLAN P10.6d1): names in scope at the cursor, members after `.`, the
//! exported names of the module inside `import { … } from "…"`, and inside `from "…"` the
//! modules an import can name (the standard library's and the `project.json` dependencies).
//!
//! The document is checked again with a placeholder name at the cursor (by the recovering
//! front end), so `p.` becomes the member access `p.<placeholder>` whose object has a type,
//! and a half-written statement is complete enough to keep its neighbours intact.

use crate::index::{self, Index};
use pico_diag::{FileId, Span};
use pico_sema::hir::{self, ExprKind, Program};
use pico_sema::{Ty, Types};
use pico_syntax::ast;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// What the cursor's name becomes while completing.
const PLACEHOLDER: &str = "__pico_cursor";

/// LSP `CompletionItemKind`s.
mod kind {
    pub const METHOD: u32 = 2;
    pub const FUNCTION: u32 = 3;
    pub const FIELD: u32 = 5;
    pub const VARIABLE: u32 = 6;
    pub const CLASS: u32 = 7;
    pub const INTERFACE: u32 = 8;
    pub const MODULE: u32 = 9;
    pub const PROPERTY: u32 = 10;
    pub const ENUM: u32 = 13;
    pub const KEYWORD: u32 = 14;
    pub const FOLDER: u32 = 19;
    pub const ENUM_MEMBER: u32 = 20;
    pub const CONSTANT: u32 = 21;
    pub const STRUCT: u32 = 22;
    pub const TYPE_PARAMETER: u32 = 25;
}

/// Built-in type names offered where any name goes.
const BUILTIN_TYPES: &[&str] = &[
    "i8", "i16", "i32", "i64", "int", "u8", "u16", "u32", "u64", "isize", "usize", "f32", "f64",
    "number", "bool", "char", "string", "void", "never",
];

#[derive(Default)]
struct Items {
    list: Vec<Value>,
    seen: HashSet<String>,
}

impl Items {
    fn add(&mut self, label: &str, kind: u32, hover: Option<&str>) {
        // (Only names that can be written: no hidden locals such as `<item>`, no statics'
        // globals `Class.field`, no internals `__…`.)
        let ident = label
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '$')
            && !label.starts_with(|c: char| c.is_ascii_digit());
        if !ident || label.starts_with("__") || !self.seen.insert(label.to_string()) {
            return;
        }
        let mut item = json!({ "label": label, "kind": kind });
        if let Some(h) = hover {
            let (sig, docs) = split_hover(h);
            item["detail"] = json!(sig);
            if !docs.is_empty() {
                item["documentation"] = json!({ "kind": "markdown", "value": docs });
            }
        }
        self.list.push(item);
    }

    /// A declaration, with the kind its hover text tells.
    fn decl(&mut self, label: &str, hover: Option<&String>) {
        let k = hover.map_or(kind::VARIABLE, |h| kind_of(split_hover(h).0));
        self.add(label, k, hover.map(String::as_str));
    }
}

/// The completion items at byte `offset` of `text`, the document at `path`. `trigger` is the
/// character that asked for them, if one did: the quotes and `/` only start a module path.
pub fn complete(path: &Path, text: &str, offset: usize, trigger: Option<&str>) -> Vec<Value> {
    let offset = offset.min(text.len());
    let dir = path.parent().unwrap_or(Path::new("."));
    if let Some((start, end)) = specifier_at(text, offset) {
        return specifiers(text, (start, offset, end), path);
    }
    if matches!(trigger, Some("\"" | "'" | "/")) {
        return Vec::new();
    }
    let start = text[..offset]
        .char_indices()
        .rev()
        .take_while(|(_, c)| c.is_alphanumeric() || *c == '_' || *c == '$')
        .last()
        .map_or(offset, |(i, _)| i);
    // The whole word is replaced: `x.‸speak()` completes the member `speak` stands for.
    let end = offset
        + text[offset..]
            .char_indices()
            .find(|(_, c)| !(c.is_alphanumeric() || *c == '_' || *c == '$'))
            .map_or(text.len() - offset, |(i, _)| i);
    if let Some(items) = import_names(text, start, dir) {
        return items;
    }
    let before = &text[..start];
    let member = before.ends_with('.') && !before.ends_with("..");
    let edited = format!("{before}{PLACEHOLDER}{}", &text[end..]);
    let checked = std::panic::catch_unwind(|| {
        pico_driver::check_source_recovering(
            path.display().to_string(),
            edited,
            dir,
            pico_driver::BuildOptions::default(),
        )
    });
    let Ok(pico_driver::Checked {
        sm,
        hir: Some(hir),
        types: Some(types),
        ..
    }) = checked
    else {
        return Vec::new();
    };
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let idx = index::build(sm, &hir, &types);
        let mut items = Items::default();
        let at = start as u32;
        if member {
            members(&mut items, &idx, &hir, &types, before, at);
        } else {
            scope(&mut items, &idx, &hir, at);
        }
        items.list
    }));
    r.unwrap_or_default()
}

/// The signature line of a hover text, and the documentation after its code block.
fn split_hover(h: &str) -> (&str, &str) {
    let body = h.strip_prefix("```pico\n").unwrap_or(h);
    match body.split_once("\n```") {
        Some((sig, rest)) => (sig, rest.trim_start()),
        None => (body, ""),
    }
}

fn kind_of(sig: &str) -> u32 {
    // `(field) P.x: i32` or `function f(…)`.
    let word = match sig.strip_prefix('(') {
        Some(rest) => rest.split(')').next().unwrap_or(""),
        None => sig.split([' ', '(']).next().unwrap_or(""),
    };
    match word {
        "function" => kind::FUNCTION,
        "method" => kind::METHOD,
        "field" => kind::FIELD,
        "property" => kind::PROPERTY,
        "class" => kind::CLASS,
        "struct" => kind::STRUCT,
        "interface" => kind::INTERFACE,
        "enum" => kind::ENUM,
        "variant" => kind::ENUM_MEMBER,
        "type" => kind::TYPE_PARAMETER,
        "const" => kind::CONSTANT,
        _ => kind::VARIABLE,
    }
}

/// The functions whose code contains `at` (in file `file`), innermost first.
fn enclosing(p: &Program, file: FileId, at: u32) -> Vec<hir::FnId> {
    let mut fs: Vec<(u32, hir::FnId)> = p
        .fns
        .iter()
        .enumerate()
        .filter(|(_, f)| f.span.file == file && f.span.start <= at && at <= f.span.end)
        .map(|(i, f)| (f.span.end - f.span.start, hir::FnId(i as u32)))
        .collect();
    fs.sort();
    fs.into_iter().map(|(_, f)| f).collect()
}

/// Members after `object.`: the object's fields, methods and getters, or a type's statics and
/// an enum's variants when `object` names one.
fn members(items: &mut Items, idx: &Index, p: &Program, t: &Types, before: &str, at: u32) {
    let file = idx.file;
    let owner = enclosing(p, file, at)
        .iter()
        .find_map(|f| p.fns[f.0 as usize].owner);
    let mut object = None;
    for f in &p.fns {
        let mut visit = |e: &hir::Expr| find_member(e, at, &mut object);
        for d in f.defaults.iter().flatten() {
            visit(d);
        }
        walk_block(&f.body, &mut visit);
    }
    if let Some(ty) = object.and_then(|id: hir::ExprId| t.exprs.get(id.0 as usize)) {
        type_members(items, idx, p, t, ty, owner);
        return;
    }
    // `Name.`: statics of a class, variants of an enum.
    let name: String = before[..before.len() - 1]
        .chars()
        .rev()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    if let Some(def) = p.structs.iter().find(|s| s.name == name) {
        for (m, g) in &def.static_fields {
            items.decl(m, idx.decls.get(&p.globals[g.0 as usize].span));
        }
        for (m, f) in def.static_methods.iter().chain(&def.static_getters) {
            items.decl(m, idx.decls.get(&p.fns[f.0 as usize].name_span));
        }
    }
    if let Some(e) = p
        .enums
        .iter()
        .find(|e| e.name == name && matches!(e.origin, hir::EnumOrigin::Declared))
    {
        for v in &e.variants {
            items.add(
                &v.name,
                kind::ENUM_MEMBER,
                idx.decls.get(&v.span).map(String::as_str),
            );
        }
    }
}

/// The member expression whose name is the placeholder at `at`.
fn find_member(e: &hir::Expr, at: u32, out: &mut Option<hir::ExprId>) {
    if let ExprKind::Member {
        object,
        name,
        name_span,
    } = &e.kind
        && name == PLACEHOLDER
        && name_span.start == at
    {
        *out = Some(object.id);
    }
    match &e.kind {
        ExprKind::Block(b) | ExprKind::Loop(b) | ExprKind::Unsafe(b) => {
            walk_block(b, &mut |c| find_member(c, at, out));
        }
        _ => e.for_each_child(&mut |c| find_member(c, at, out)),
    }
}

fn walk_block(b: &hir::Block, f: &mut dyn FnMut(&hir::Expr)) {
    for s in &b.stmts {
        match s {
            hir::Stmt::Let { init: Some(i), .. } => f(i),
            hir::Stmt::Let { .. } => {}
            hir::Stmt::Expr(e) => f(e),
        }
    }
    if let Some(t) = &b.tail {
        f(t);
    }
}

fn type_members(
    items: &mut Items,
    idx: &Index,
    p: &Program,
    t: &Types,
    ty: &Ty,
    owner: Option<hir::StructId>,
) {
    let ty = match ty {
        Ty::Nullable(t) | Ty::Immutable(t) => t,
        t => t,
    };
    if let Some(sid) = index::struct_of(ty) {
        let def = &p.structs[sid.0 as usize];
        let visible = |name: &str| owner == Some(sid) || !def.private.contains(name);
        for f in &def.fields {
            if visible(&f.name) {
                items.add(
                    &f.name,
                    kind::FIELD,
                    idx.decls.get(&f.span).map(String::as_str),
                );
            }
        }
        for (name, f) in &def.getters {
            if visible(name) {
                let sig = index::fn_sig(p, t, *f);
                let h = idx.decls.get(&p.fns[f.0 as usize].name_span).cloned();
                items.add(name, kind::PROPERTY, Some(h.as_deref().unwrap_or(&sig)));
            }
        }
        for (name, f) in &def.methods {
            if visible(name) && name != "constructor" {
                let h = idx.decls.get(&p.fns[f.0 as usize].name_span);
                items.add(name, kind::METHOD, h.map(String::as_str));
            }
        }
        return;
    }
    let builtin = match ty {
        Ty::Array(_) => "Array",
        Ty::Str => "String",
        Ty::Char => "Char",
        Ty::Fixed(..) | Ty::FixedN(..) => "Fixed",
        _ => "",
    };
    // (Members the type checker implements itself.)
    for (on, name, sig, docs) in hir::BUILTIN_MEMBER_DOCS {
        if *on == builtin {
            let k = if sig.starts_with("(property)") {
                kind::PROPERTY
            } else {
                kind::METHOD
            };
            let docs = pico_syntax::docs::for_display(docs);
            items.add(name, k, Some(&format!("```pico\n{sig}\n```\n\n{docs}")));
        }
    }
    let mut ext: Vec<(&String, &Vec<hir::FnId>)> = p
        .extensions
        .iter()
        .filter(|((on, _), _)| on == builtin)
        .map(|((_, m), fs)| (m, fs))
        .collect();
    ext.sort();
    for (m, fs) in ext {
        if let Some(f) = fs.first() {
            let h = idx.decls.get(&p.fns[f.0 as usize].name_span);
            items.add(m, kind::METHOD, h.map(String::as_str));
        }
    }
    if matches!(
        ty,
        Ty::Int(_) | Ty::Float(_) | Ty::Bool | Ty::Char | Ty::Str
    ) {
        items.add(
            "toString",
            kind::METHOD,
            Some("```pico\n(method) toString(): string\n```"),
        );
    }
}

/// Names in scope at `at`: locals of the functions around it, the file's top-level
/// declarations and imports, the core library, built-ins and keywords.
fn scope(items: &mut Items, idx: &Index, p: &Program, at: u32) {
    let file = idx.file;
    for fid in enclosing(p, file, at) {
        let f = &p.fns[fid.0 as usize];
        // The block each `let`/`const` is declared in: out of scope outside it.
        let mut blocks: HashMap<hir::LocalId, Span> = HashMap::new();
        let_blocks(&f.body, &mut blocks);
        for (i, b) in f.locals.iter().enumerate() {
            let id = hir::LocalId(i as u32);
            let in_block = blocks.get(&id).is_none_or(|s| s.start <= at && at <= s.end);
            if b.span.file == file && b.span.start < at && in_block && b.name != PLACEHOLDER {
                items.decl(&b.name, idx.decls.get(&b.span));
            }
        }
    }
    for g in &p.globals {
        if g.span.file == file && g.name != PLACEHOLDER {
            items.decl(&g.name, idx.decls.get(&g.span));
        }
    }
    let prelude = |s: Span| idx.sm.file(s.file).name.starts_with("<core>/");
    // (`extern "C"` functions of the core library are its internals.)
    let externs: HashSet<hir::FnId> = p.externs.iter().map(|e| e.func).collect();
    let fns = p.fns.iter().enumerate().filter(|(i, f)| {
        !f.is_entry
            && f.closure.is_none()
            && f.this.is_none()
            && f.owner.is_none()
            && !(externs.contains(&hir::FnId(*i as u32)) && prelude(f.name_span))
    });
    // (The driver takes the imports out of the module: parse the file's again.)
    let (own, _) = pico_syntax::parse_module(file, &idx.sm.file(file).text);
    // The file's own declarations, then the imported names, then the core library's.
    for pass in [true, false] {
        let mine = |s: Span| if pass { s.file == file } else { prelude(s) };
        for (_, f) in fns.clone().filter(|(_, f)| mine(f.name_span)) {
            items.decl(&f.name, idx.decls.get(&f.name_span));
        }
        for s in p.structs.iter().filter(|s| mine(s.name_span)) {
            items.decl(&s.name, idx.decls.get(&s.name_span));
        }
        for e in p.enums.iter().filter(|e| mine(e.name_span)) {
            if matches!(e.origin, hir::EnumOrigin::Declared) {
                items.decl(&e.name, idx.decls.get(&e.name_span));
            }
        }
        if pass {
            for stmt in &own.stmts {
                top_level_names(items, idx, p, stmt);
            }
        } else {
            for g in p.globals.iter().filter(|g| prelude(g.span)) {
                items.decl(&g.name, idx.decls.get(&g.span));
            }
        }
    }
    // Built-in functions (those with pico declarations come from the core library).
    for (name, sig, docs) in pico_sema::hir::BUILTIN_DOCS {
        let docs = pico_syntax::docs::for_display(docs);
        items.add(
            name,
            kind::FUNCTION,
            Some(&format!("```pico\n{sig}\n```\n\n{docs}")),
        );
    }
    for t in BUILTIN_TYPES {
        items.add(t, kind::KEYWORD, None);
    }
    for k in pico_syntax::token::Kw::ALL {
        items.add(k.as_str(), kind::KEYWORD, None);
    }
}

/// Aliases and imports of the file (its other declarations are in the HIR).
fn top_level_names(items: &mut Items, idx: &Index, p: &Program, stmt: &ast::Stmt) {
    let ast::StmtKind::Decl(d) = &stmt.kind else {
        return;
    };
    match &d.kind {
        ast::DeclKind::TypeAlias { name, .. } => {
            let line = idx.sm.snippet(stmt.span).lines().next().unwrap_or("");
            let line = line.strip_prefix("export ").unwrap_or(line);
            items.add(
                &name.name,
                kind::TYPE_PARAMETER,
                Some(&format!("```pico\n{line}\n```")),
            );
        }
        ast::DeclKind::Import(i) => match &i.kind {
            ast::ImportKind::Named(names) => {
                for n in names {
                    let local = n.alias.as_ref().unwrap_or(&n.name);
                    let def = p.name_refs.iter().find(|(u, _)| *u == n.name.span);
                    items.decl(&local.name, def.and_then(|(_, d)| idx.decls.get(d)));
                }
            }
            ast::ImportKind::Namespace(alias) => {
                let h = format!(
                    "```pico\nimport * as {} from \"{}\"\n```",
                    alias.name, i.from
                );
                items.add(&alias.name, kind::MODULE, Some(&h));
            }
        },
        _ => {}
    }
}

/// Records the block each `let`/`const` of a function body is declared in.
fn let_blocks(b: &hir::Block, out: &mut HashMap<hir::LocalId, Span>) {
    fn expr(e: &hir::Expr, out: &mut HashMap<hir::LocalId, Span>) {
        match &e.kind {
            ExprKind::Block(b) | ExprKind::Loop(b) | ExprKind::Unsafe(b) => let_blocks(b, out),
            ExprKind::Arena { body, .. } => let_blocks(body, out),
            _ => e.for_each_child(&mut |c| expr(c, out)),
        }
    }
    for s in &b.stmts {
        match s {
            hir::Stmt::Let { var, init, .. } => {
                if let hir::Var::Local(l) = var {
                    out.insert(*l, b.span);
                }
                if let Some(i) = init {
                    expr(i, out);
                }
            }
            hir::Stmt::Expr(e) => expr(e, out),
        }
    }
    if let Some(t) = &b.tail {
        expr(t, out);
    }
}

/// Inside `import { … } from "spec"`: the module's exported names (`None` elsewhere).
fn import_names(text: &str, at: usize, dir: &Path) -> Option<Vec<Value>> {
    text[..at].rfind("import").filter(|&i| {
        let between = &text[i + "import".len()..at];
        between.trim_start().starts_with('{') && !between.contains('}') && !between.contains(';')
    })?;
    let after = &text[at..];
    let close = after.find('}')?;
    let tail = after[close + 1..]
        .trim_start()
        .strip_prefix("from")?
        .trim_start();
    // (Either quote: `from "./x"` or `from './x'`.)
    let quote = tail.chars().next().filter(|c| matches!(c, '"' | '\''))?;
    let spec = tail[1..].split(quote).next()?;
    let path = module_path(spec, dir)?;
    let src = std::fs::read_to_string(&path).ok()?;
    let (m, _) = pico_syntax::parse_module(FileId(0), &src);
    let mut items = Items::default();
    for stmt in &m.stmts {
        let ast::StmtKind::Decl(d) = &stmt.kind else {
            continue;
        };
        let (name, k) = match &d.kind {
            _ if !d.exported && !matches!(d.kind, ast::DeclKind::ExportList { .. }) => continue,
            ast::DeclKind::Function(f) => match &f.name {
                Some(n) => (n.name.clone(), kind::FUNCTION),
                None => continue,
            },
            ast::DeclKind::Struct(s) => (s.name.name.clone(), kind::STRUCT),
            ast::DeclKind::Class(c) => (c.name.name.clone(), kind::CLASS),
            ast::DeclKind::Interface(i) => (i.name.name.clone(), kind::INTERFACE),
            ast::DeclKind::Enum(e) => (e.name.name.clone(), kind::ENUM),
            ast::DeclKind::TypeAlias { name, .. } => (name.name.clone(), kind::TYPE_PARAMETER),
            ast::DeclKind::Binding(b) => {
                if let ast::StmtKind::Let { pat, .. } = &b.kind
                    && let ast::PatternKind::Ident(n) = &pat.kind
                {
                    (n.name.clone(), kind::CONSTANT)
                } else {
                    continue;
                }
            }
            ast::DeclKind::ExportList { names, .. } => {
                for n in names {
                    let local = n.alias.as_ref().unwrap_or(&n.name);
                    items.add(&local.name, kind::VARIABLE, None);
                }
                continue;
            }
            _ => continue,
        };
        let line = src
            .get(stmt.span.start as usize..stmt.span.end as usize)
            .and_then(|s| s.lines().next())
            .unwrap_or("");
        let line = line.strip_prefix("export ").unwrap_or(line);
        let line = line.trim_end_matches('{').trim_end();
        items.add(&name, k, Some(&format!("```pico\n{line}\n```")));
    }
    Some(items.list)
}

/// Inside the string of `from "…"` (an import or a re-export): the byte range of the module
/// path around `at`, without the quotes (`None` elsewhere).
fn specifier_at(text: &str, at: usize) -> Option<(usize, usize)> {
    let line = text[..at].rfind('\n').map_or(0, |i| i + 1);
    let open = line + text[line..at].rfind(['"', '\''])?;
    let quote = text[open..].chars().next()?;
    let lead = text[..open].trim_end().strip_suffix("from")?;
    if lead.ends_with(|c: char| c.is_alphanumeric() || c == '_' || c == '$') {
        return None;
    }
    let rest = &text[at..];
    let end = at
        + rest
            .find(|c: char| c == quote || c == ';' || c.is_whitespace())
            .unwrap_or(rest.len());
    Some((open + 1, end))
}

/// The modules an import can name: once the path starts with `.`, the files and directories
/// it leads to; before that the standard library's modules, the dependencies in the package's
/// `project.json` and the modules next to this one. Each replaces the whole path between the
/// quotes (`start..end`; the cursor is at `at`).
fn specifiers(text: &str, (start, at, end): (usize, usize, usize), path: &Path) -> Vec<Value> {
    let range = json!({ "start": lsp_position(text, start), "end": lsp_position(text, end) });
    let dir = path.parent().unwrap_or(Path::new("."));
    let mut items = Vec::new();
    let mut add = |label: &str, k: u32, detail: &str, docs: Option<String>| {
        let mut item = json!({
            "label": label,
            "kind": k,
            "detail": detail,
            "filterText": label,
            "textEdit": { "range": range, "newText": label },
        });
        if let Some(d) = docs.filter(|d| !d.is_empty()) {
            item["documentation"] = json!({ "kind": "markdown", "value": d });
        }
        if k == kind::FOLDER {
            // (Its contents next.)
            item["command"] = json!({ "title": "", "command": "editor.action.triggerSuggest" });
        }
        items.push(item);
    };
    let typed = &text[start..at];
    if typed.starts_with('.') {
        // `../lib/ge‸`: the entries of `../lib/`; `.‸` and `..‸`: those of `./` and `../`.
        let (base, folders): (&str, &[&str]) = match typed.rfind('/') {
            Some(i) => (&typed[..=i], &[]),
            None => ("./", &["./", "../"]),
        };
        for f in folders {
            add(f, kind::FOLDER, "directory", None);
        }
        for (name, k, file) in relative_entries(&dir.join(base), path) {
            let detail = if k == kind::FOLDER {
                "directory"
            } else {
                "module"
            };
            add(
                &format!("{base}{name}"),
                k,
                detail,
                file.and_then(|f| module_doc(&f)),
            );
        }
        return items;
    }
    for (name, file) in std_modules() {
        let docs = module_doc(&file);
        add(
            &format!("std/{name}"),
            kind::MODULE,
            "standard library",
            docs,
        );
    }
    if let Some(Ok(m)) = pico_driver::packages::package_of(dir) {
        for (deps, what) in [
            (&m.dependencies, "dependency"),
            (&m.dev_dependencies, "dev dependency"),
        ] {
            for (name, source) in deps {
                let detail = format!("{what}: {}", source_label(source));
                let docs = module_path(name, dir).and_then(|f| module_doc(&f));
                add(name, kind::MODULE, &detail, docs);
            }
        }
    }
    if typed.is_empty() {
        for (name, k, file) in relative_entries(dir, path) {
            if k == kind::MODULE {
                add(
                    &format!("./{name}"),
                    k,
                    "module",
                    file.and_then(|f| module_doc(&f)),
                );
            }
        }
    }
    items
}

/// What a relative import can name in `dir`: its `.pi` modules (without the extension; not
/// `self`, the document, nor test and benchmark files), its directories with an `index.pi`
/// as modules, and its directories (`name/`, not hidden ones) to go into. Sorted by name.
fn relative_entries(dir: &Path, self_: &Path) -> Vec<(String, u32, Option<std::path::PathBuf>)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let me = self_.canonicalize().ok();
    let mut out = Vec::new();
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let p = e.path();
        if name.starts_with('.') {
            continue;
        }
        if p.is_dir() {
            let index = p.join("index.pi");
            if index.is_file() {
                out.push((name.clone(), kind::MODULE, Some(index)));
            }
            out.push((format!("{name}/"), kind::FOLDER, None));
        } else if let Some(stem) = name.strip_suffix(".pi")
            && !stem.ends_with(".test")
            && !stem.ends_with(".bench")
            && p.canonicalize().ok() != me
        {
            out.push((stem.to_string(), kind::MODULE, Some(p)));
        }
    }
    out.sort();
    out
}

/// The standard library's modules, by name, and their files: `x.pi` and `x/index.pi` (not the
/// core library, which every module has, nor its tests).
fn std_modules() -> Vec<(String, std::path::PathBuf)> {
    let Ok(entries) = std::fs::read_dir(pico_driver::stdlib_dir()) else {
        return Vec::new();
    };
    let mut mods: Vec<(String, std::path::PathBuf)> = entries
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            if path.is_dir() {
                let name = e.file_name().to_string_lossy().into_owned();
                let index = path.join("index.pi");
                (!matches!(name.as_str(), "core" | "tests") && index.is_file())
                    .then_some((name, index))
            } else if path.extension().is_some_and(|x| x == "pi") {
                let name = path.file_stem()?.to_string_lossy().into_owned();
                // (`std/greeting` is there for the toolchain's own tests.)
                (name != "greeting").then_some((name, path))
            } else {
                None
            }
        })
        .collect();
    mods.sort();
    mods
}

/// Where a dependency comes from, as its completion's detail.
fn source_label(source: &pico_driver::packages::Source) -> String {
    use pico_driver::packages::{GitPin, Source};
    match source {
        Source::Range(r) => r.to_string(),
        Source::Git { url, pin } => match pin {
            GitPin::Tag(t) => format!("{url} (tag {t})"),
            GitPin::Branch(b) => format!("{url} (branch {b})"),
            GitPin::Rev(r) => format!("{url} (rev {r})"),
            GitPin::Default => url.clone(),
        },
    }
}

/// The comment a module starts with (`// …` or `//! …` lines before its first declaration).
fn module_doc(file: &Path) -> Option<String> {
    let src = std::fs::read_to_string(file).ok()?;
    let lines: Vec<&str> = src
        .lines()
        .map(str::trim_start)
        .take_while(|l| l.starts_with("//") && !l.starts_with("///"))
        .map(|l| {
            let l = l.trim_start_matches("//");
            let l = l.strip_prefix('!').unwrap_or(l);
            l.strip_prefix(' ').unwrap_or(l)
        })
        .collect();
    Some(lines.join("\n").trim().to_string()).filter(|d| !d.is_empty())
}

/// The LSP position (line, UTF-16 code units) of byte `offset` of `text`.
fn lsp_position(text: &str, offset: usize) -> Value {
    let before = &text[..offset];
    let line_start = before.rfind('\n').map_or(0, |i| i + 1);
    let character: usize = before[line_start..].chars().map(char::len_utf16).sum();
    json!({ "line": before.matches('\n').count(), "character": character })
}

/// The file an import specifier names (as the driver resolves it).
fn module_path(spec: &str, dir: &Path) -> Option<std::path::PathBuf> {
    let base = if let Some(rest) = spec.strip_prefix("std/") {
        pico_driver::stdlib_dir().join(rest)
    } else if spec.starts_with("./") || spec.starts_with("../") {
        dir.join(spec)
    } else if pico_driver::packages::is_package_name(spec) {
        // A package: its `index.pi` (spec §8.3).
        let index = pico_driver::packages::resolve(
            spec,
            dir,
            None,
            Some(&pico_driver::packages::prepare(dir)),
        )
        .ok()?;
        return Some(index.with_extension("pi")).filter(|p| p.is_file());
    } else {
        return None;
    };
    let candidates = [
        base.clone(),
        base.with_extension("pi"),
        base.join("index.pi"),
    ];
    candidates.into_iter().find(|p| p.is_file())
}
