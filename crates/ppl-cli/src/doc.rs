//! `ppl doc` (spec §14.1, PLAN P10.9b): HTML API docs from doc comments (`/** … */`, `///`).
//!
//! A page per module: its exported declarations (for the core library, its public globals),
//! each with its signature as written (the source up to its body) and its doc comment rendered
//! from Markdown, then the public members of types. The core library's methods of built-in
//! types (`__Array_map(xs, f)`) are pages of their own (`Array`, with `map(f)`). Type names in
//! signatures link to their declarations. `ppl doc --std` documents the standard and core
//! libraries; `ppl doc` the current project's package (its `index.ppl`).

use ppl_diag::FileId;
use ppl_syntax::ast::{self, DeclKind, MemberKind, StmtKind, Visibility};
use ppl_syntax::docs::doc_comment;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// One documented declaration (or member).
#[derive(Clone)]
struct Item {
    /// What it's called on the page (its anchor too).
    name: String,
    kind: &'static str,
    signature: String,
    docs: String,
    members: Vec<Item>,
}

/// A page: a module (`std/path`), the core library, or a built-in type's methods.
struct Page {
    title: String,
    file: String,
    items: Vec<Item>,
}

pub fn run(std: bool, out: Option<&Path>) -> ExitCode {
    let pages = if std {
        std_pages()
    } else {
        match project_pages() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("error: {e}");
                return ExitCode::from(2);
            }
        }
    };
    let out = out
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("target/doc"));
    match write_site(
        &out,
        &pages,
        if std {
            "The ppl standard library"
        } else {
            "API"
        },
    ) {
        Ok(()) => {
            let n: usize = pages.iter().map(|p| p.items.len()).sum();
            println!(
                "documented {n} declarations in {} pages: {}",
                pages.len(),
                out.join("index.html").display()
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: cannot write {}: {e}", out.display());
            ExitCode::FAILURE
        }
    }
}

/// The standard library's modules (`std/…`, their exports) and the core library (its public
/// globals, and the built-in types' methods).
fn std_pages() -> Vec<Page> {
    let dir = ppl_driver::stdlib_dir();
    // The core library first: std modules re-export its internal declarations under public
    // names (`export { __ClassDecl as ClassDecl }`).
    let mut core_files: Vec<PathBuf> = std::fs::read_dir(dir.join("core"))
        .map(|r| r.filter_map(|e| Some(e.ok()?.path())).collect())
        .unwrap_or_default();
    core_files.retain(|p| p.extension().is_some_and(|e| e == "ppl"));
    core_files.sort();
    let core_items: Vec<Item> = core_files
        .iter()
        .flat_map(|f| module_items(f, Exports::Core, &BTreeMap::new(), &mut BTreeMap::new()))
        .collect();
    let core_by_name: BTreeMap<String, Item> = core_items
        .iter()
        .map(|i| (i.name.clone(), i.clone()))
        .collect();
    let mut renames = BTreeMap::new();
    let mut pages = Vec::new();
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .map(|r| r.filter_map(|e| Some(e.ok()?.path())).collect())
        .unwrap_or_default();
    files.retain(|p| p.extension().is_some_and(|e| e == "ppl"));
    files.sort();
    for f in files {
        let stem = f
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        let items = module_items(&f, Exports::Exported, &core_by_name, &mut renames);
        if !items.is_empty() {
            pages.push(Page {
                title: format!("std/{stem}"),
                file: format!("std-{stem}.html"),
                items,
            });
        }
    }
    // The core library: its globals, and the methods of built-in types.
    let mut core = Vec::new();
    let mut methods: BTreeMap<String, Vec<Item>> = BTreeMap::new();
    for item in core_items {
        match builtin_method(&item) {
            Some((ty, m)) => methods.entry(ty).or_default().push(m),
            None if !item.name.starts_with("__") => core.push(item),
            None => {}
        }
    }
    // The members of `Array` and `String` the compiler implements (`push`, `length`, …).
    for (on, name, sig, docs) in ppl_sema::hir::BUILTIN_MEMBER_DOCS {
        let Some(page) = methods.get_mut(*on) else {
            continue;
        };
        // (`(method) Array.push(…)` → `push(…)`, `(property) length: i64` → `length: i64`.)
        let (kind, sig) = match sig.strip_prefix("(property) ") {
            Some(s) => ("property", s.to_string()),
            None => {
                let s = sig.strip_prefix("(method) ").unwrap_or(sig);
                (
                    "method",
                    s.split_once('.').map_or(s, |(_, m)| m).to_string(),
                )
            }
        };
        page.push(Item {
            name: name.to_string(),
            kind,
            signature: sig,
            docs: docs.to_string(),
            members: Vec::new(),
        });
    }
    // The functions the compiler implements (`println`, …).
    for (name, sig, docs) in ppl_sema::hir::BUILTIN_DOCS {
        core.push(Item {
            name: name.to_string(),
            kind: "function",
            signature: sig.to_string(),
            docs: docs.to_string(),
            members: Vec::new(),
        });
    }
    core.sort_by(|a, b| a.name.cmp(&b.name));
    pages.insert(
        0,
        Page {
            title: "core".into(),
            file: "core.html".into(),
            items: core,
        },
    );
    for (ty, mut items) in methods {
        items.sort_by(|a, b| a.name.cmp(&b.name));
        pages.insert(
            1,
            Page {
                title: ty.clone(),
                file: format!("type-{ty}.html"),
                items,
            },
        );
    }
    // Internal names in signatures as their modules export them (`__ClassDecl` → `ClassDecl`).
    for p in &mut pages {
        for i in &mut p.items {
            rename_in(i, &renames);
        }
    }
    pages
}

fn rename_in(item: &mut Item, renames: &BTreeMap<String, String>) {
    item.signature = rename_words(&item.signature, renames);
    for m in &mut item.members {
        rename_in(m, renames);
    }
}

/// `s` with each whole word that `renames` names replaced.
fn rename_words(s: &str, renames: &BTreeMap<String, String>) -> String {
    let mut out = String::new();
    let mut word = String::new();
    for c in s.chars().chain(std::iter::once('\0')) {
        if c.is_alphanumeric() || c == '_' {
            word.push(c);
            continue;
        }
        out.push_str(renames.get(&word).unwrap_or(&word));
        word.clear();
        if c != '\0' {
            out.push(c);
        }
    }
    out
}

/// The current project's package: its `index.ppl`.
fn project_pages() -> Result<Vec<Page>, String> {
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let m =
        match ppl_driver::packages::package_of(&cwd) {
            Some(Ok(m)) => m,
            Some(Err(e)) => return Err(format!("project.json is invalid: {}", e.join("; "))),
            None => return Err(
                "no project.json here or above (`ppl doc --std` documents the standard library)"
                    .into(),
            ),
        };
    let index = m.dir.join("index.ppl");
    if !index.is_file() {
        return Err(format!(
            "{} has no index.ppl: a package's API is its index.ppl",
            m.dir.display()
        ));
    }
    Ok(vec![Page {
        title: m.label(),
        file: "index-api.html".into(),
        items: module_items(
            &index,
            Exports::Exported,
            &BTreeMap::new(),
            &mut BTreeMap::new(),
        ),
    }])
}

#[derive(Clone, Copy, PartialEq)]
enum Exports {
    /// A module's `export`ed declarations.
    Exported,
    /// The core library's top-level declarations (its globals; `__` names are internal).
    Core,
}

/// The documented declarations of a file: its `export`ed ones and those an export list names
/// (`export { a, __b as b }`: declared in the file or, for std modules, in the core library
/// `core`; each renaming is added to `renames`).
fn module_items(
    path: &Path,
    which: Exports,
    core: &BTreeMap<String, Item>,
    renames: &mut BTreeMap<String, String>,
) -> Vec<Item> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let (module, _) = ppl_syntax::parse_module(FileId(0), &text);
    let mut out = Vec::new();
    let mut local = BTreeMap::new();
    let mut lists = Vec::new();
    for s in &module.stmts {
        let StmtKind::Decl(d) = &s.kind else {
            continue;
        };
        if let DeclKind::ExportList { names, from: None } = &d.kind {
            lists.extend(names.iter().cloned());
            continue;
        }
        let Some(item) = decl_item(&text, d) else {
            continue;
        };
        if which == Exports::Core || d.exported {
            out.push(item);
        } else {
            local.insert(item.name.clone(), item);
        }
    }
    for n in lists {
        let Some(mut item) = local
            .get(&n.name.name)
            .or_else(|| core.get(&n.name.name))
            .cloned()
        else {
            continue;
        };
        if let Some(alias) = &n.alias {
            renames.insert(item.name.clone(), alias.name.clone());
            item.name = alias.name.clone();
        }
        out.push(item);
    }
    out
}

fn slice(text: &str, start: u32, end: u32) -> &str {
    text.get(start as usize..end as usize).unwrap_or("")
}

/// A signature from the source: up to where the body starts, `export` dropped, one line.
fn signature(text: &str, start: u32, body: Option<u32>, end: u32) -> String {
    let s = slice(text, start, body.unwrap_or(end));
    let s = s.trim().trim_end_matches('{').trim();
    let s = s.strip_prefix("export ").unwrap_or(s);
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn fn_body_start(f: &ast::FnDecl) -> Option<u32> {
    match &f.body {
        Some(ast::FnBody::Block(b)) => Some(b.span.start),
        Some(ast::FnBody::Expr(e)) => Some(e.span.start),
        None => None,
    }
}

fn decl_item(text: &str, d: &ast::Decl) -> Option<Item> {
    let docs = doc_comment(text, d.span.start);
    let (name, kind, sig, members, vis) = match &d.kind {
        DeclKind::Function(f) => {
            let name = f.name.as_ref()?.name.clone();
            let sig = signature(text, d.span.start, fn_body_start(f), d.span.end);
            (name, "function", sig, &[][..], Vis::All)
        }
        DeclKind::Class(c) => (
            c.name.name.clone(),
            "class",
            header(text, d),
            &c.members[..],
            Vis::Public,
        ),
        DeclKind::Struct(s) => (
            s.name.name.clone(),
            "struct",
            header(text, d),
            &s.members[..],
            Vis::UnlessPrivate,
        ),
        DeclKind::Interface(i) => (
            i.name.name.clone(),
            "interface",
            header(text, d),
            &i.members[..],
            Vis::All,
        ),
        DeclKind::Enum(e) => (
            e.name.name.clone(),
            "enum",
            slice(text, d.span.start, d.span.end)
                .trim()
                .trim_start_matches("export ")
                .to_string(),
            &e.members[..],
            Vis::UnlessPrivate,
        ),
        DeclKind::TypeAlias { name, .. } => (
            name.name.clone(),
            "type",
            signature(text, d.span.start, None, d.span.end),
            &[][..],
            Vis::All,
        ),
        DeclKind::Binding(b) => {
            let StmtKind::Let { pat, init, .. } = &b.kind else {
                return None;
            };
            let ast::PatternKind::Ident(n) = &pat.kind else {
                return None;
            };
            // (The binding without its initializer: `const delimiter: string`.)
            let end = init.as_ref().map_or(d.span.end, |i| i.span.start);
            let sig = signature(text, d.span.start, Some(end), d.span.end);
            let sig = sig.trim_end_matches('=').trim().to_string();
            (n.name.clone(), "constant", sig, &[][..], Vis::All)
        }
        _ => return None,
    };
    let members = members
        .iter()
        .filter_map(|m| member_item(text, m, vis))
        .collect();
    Some(Item {
        name,
        kind,
        signature: sig,
        docs,
        members,
    })
}

#[derive(Clone, Copy)]
enum Vis {
    All,
    UnlessPrivate,
    Public,
}

/// A type's header: `class Map<K extends Hashable<K>, V>` (up to its body's brace).
fn header(text: &str, d: &ast::Decl) -> String {
    let s = slice(text, d.span.start, d.span.end);
    let head = s.split('{').next().unwrap_or(s);
    let head = head.trim().strip_prefix("export ").unwrap_or(head.trim());
    head.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn member_item(text: &str, m: &ast::Member, vis: Vis) -> Option<Item> {
    // (A class's constructors are callable unless `private`; its other members only when
    // `public`.)
    let ctor = matches!(m.kind, MemberKind::Constructor(_));
    let public = match (vis, m.mods.visibility) {
        (Vis::All, _) => true,
        (Vis::UnlessPrivate, v) => v != Some(Visibility::Private),
        (Vis::Public, v) if ctor => v != Some(Visibility::Private),
        (Vis::Public, v) => v == Some(Visibility::Public),
    };
    if !public {
        return None;
    }
    let (name, kind, sig) = match &m.kind {
        MemberKind::Field { name, .. } => (
            name.name.clone(),
            "field",
            signature(text, m.span.start, None, m.span.end),
        ),
        MemberKind::Method(f) | MemberKind::Getter(f) | MemberKind::Setter(f) => {
            let name = f.name.as_ref()?.name.clone();
            let kind = match &m.kind {
                MemberKind::Method(_) => "method",
                _ => "property",
            };
            (
                name,
                kind,
                signature(text, m.span.start, fn_body_start(f), m.span.end),
            )
        }
        MemberKind::Constructor(f) => (
            "constructor".into(),
            "constructor",
            signature(text, m.span.start, fn_body_start(f), m.span.end),
        ),
        _ => return None,
    };
    if name.starts_with("__") {
        return None;
    }
    Some(Item {
        name,
        kind,
        signature: sig,
        docs: doc_comment(text, m.span.start),
        members: Vec::new(),
    })
}

/// A core library function that is a built-in type's method: `__Array_map(xs: T[], f)` is
/// `Array`'s `map(f)`.
fn builtin_method(item: &Item) -> Option<(String, Item)> {
    let rest = item.name.strip_prefix("__")?;
    let (ty, method) = rest.split_once('_')?;
    if !matches!(ty, "Array" | "String") || item.kind != "function" {
        return None;
    }
    // `function __Array_map<T>(xs: T[], f: …): U[]` → `map<T>(f: …): U[]`.
    let sig = item
        .signature
        .strip_prefix("function ")
        .unwrap_or(&item.signature);
    let sig = sig.replacen(&item.name, method, 1);
    let sig = match (sig.find('('), first_param_end(&sig)) {
        (Some(open), Some(end)) => format!(
            "{}{}",
            &sig[..=open],
            sig[end..].trim_start_matches(',').trim_start()
        ),
        _ => sig,
    };
    Some((
        ty.to_string(),
        Item {
            name: method.to_string(),
            kind: "method",
            signature: sig,
            docs: item.docs.clone(),
            members: Vec::new(),
        },
    ))
}

/// Where the first parameter ends (the `,` or `)` after it, at depth 0).
fn first_param_end(sig: &str) -> Option<usize> {
    let open = sig.find('(')?;
    let mut depth = 0;
    for (i, c) in sig[open + 1..].char_indices() {
        match c {
            '(' | '[' | '<' | '{' => depth += 1,
            ')' if depth == 0 => return Some(open + 1 + i),
            ')' | ']' | '>' | '}' => depth -= 1,
            ',' if depth == 0 => return Some(open + 1 + i),
            _ => {}
        }
    }
    None
}

// ---------------------------------------------------------------- HTML

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn markdown(md: &str) -> String {
    let mut html = String::new();
    pulldown_cmark::html::push_html(&mut html, pulldown_cmark::Parser::new(md));
    html
}

/// A signature as HTML, its type names linked to their pages (`links`: name → URL).
fn linked(sig: &str, links: &BTreeMap<String, String>, own: &str) -> String {
    let mut out = String::new();
    let mut word = String::new();
    let flush = |word: &mut String, out: &mut String| {
        if !word.is_empty() {
            match links.get(word.as_str()) {
                Some(url) if word != own => {
                    let _ = write!(out, "<a href=\"{url}\">{}</a>", esc(word));
                }
                _ => out.push_str(&esc(word)),
            }
            word.clear();
        }
    };
    for c in sig.chars() {
        if c.is_alphanumeric() || c == '_' {
            word.push(c);
        } else {
            flush(&mut word, &mut out);
            out.push_str(&esc(&c.to_string()));
        }
    }
    flush(&mut word, &mut out);
    out
}

const CSS: &str = r#"
:root { --bg: #fff; --fg: #1d1d1f; --muted: #6b6b70; --line: #e4e4e7; --code: #f4f4f5; --link: #2457c5; }
@media (prefers-color-scheme: dark) {
  :root { --bg: #17171a; --fg: #e6e6e9; --muted: #9a9aa2; --line: #2c2c31; --code: #222227; --link: #8ab0ff; }
}
body { margin: 0; background: var(--bg); color: var(--fg); font: 16px/1.55 system-ui, sans-serif; }
main { max-width: 860px; margin: 0 auto; padding: 24px 16px 64px; }
nav { font-size: 14px; color: var(--muted); margin-bottom: 8px; }
a { color: var(--link); text-decoration: none; } a:hover { text-decoration: underline; }
h1 { font-size: 28px; margin: 8px 0 20px; } h2 { font-size: 18px; margin: 32px 0 8px; }
pre, code { font: 14px/1.5 ui-monospace, SFMono-Regular, Menlo, monospace; }
pre.sig { background: var(--code); padding: 10px 12px; border-radius: 6px; overflow-x: auto; margin: 0 0 8px; white-space: pre-wrap; }
p code, li code { background: var(--code); padding: 1px 4px; border-radius: 4px; }
.kind { color: var(--muted); font-size: 13px; font-weight: normal; margin-left: 6px; }
.members { margin-left: 18px; padding-left: 14px; border-left: 2px solid var(--line); }
.members h3 { font-size: 15px; margin: 18px 0 6px; }
ul.toc { columns: 2; padding-left: 18px; } .none { color: var(--muted); }
"#;

fn page_html(title: &str, back: bool, body: &str) -> String {
    let nav = if back {
        "<nav><a href=\"index.html\">← all modules</a></nav>"
    } else {
        ""
    };
    format!(
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width, initial-scale=1\"><title>{t}</title><style>{CSS}</style></head><body><main>{nav}<h1>{t}</h1>\n{body}</main></body></html>\n",
        t = esc(title)
    )
}

fn write_site(out: &Path, pages: &[Page], title: &str) -> std::io::Result<()> {
    std::fs::create_dir_all(out)?;
    // Where each type is documented (for links in signatures).
    let mut links = BTreeMap::new();
    for p in pages {
        for i in &p.items {
            if matches!(i.kind, "class" | "struct" | "interface" | "enum" | "type") {
                links
                    .entry(i.name.clone())
                    .or_insert(format!("{}#{}", p.file, i.name));
            }
        }
        if p.file.starts_with("type-") {
            links.entry(p.title.clone()).or_insert(p.file.clone());
        }
    }
    let mut index = String::from("<ul>\n");
    for p in pages {
        let _ = writeln!(
            index,
            "<li><a href=\"{}\">{}</a> <span class=\"kind\">{} declarations</span></li>",
            p.file,
            esc(&p.title),
            p.items.len()
        );
        let mut body = String::from("<ul class=\"toc\">\n");
        for i in &p.items {
            let _ = writeln!(body, "<li><a href=\"#{n}\">{n}</a></li>", n = esc(&i.name));
        }
        body.push_str("</ul>\n");
        for i in &p.items {
            item_html(&mut body, i, &links, "h2");
        }
        std::fs::write(out.join(&p.file), page_html(&p.title, true, &body))?;
    }
    index.push_str("</ul>\n");
    std::fs::write(out.join("index.html"), page_html(title, false, &index))
}

fn item_html(out: &mut String, i: &Item, links: &BTreeMap<String, String>, h: &str) {
    let _ = writeln!(
        out,
        "<{h} id=\"{n}\">{n}<span class=\"kind\">{k}</span></{h}>\n<pre class=\"sig\">{s}</pre>",
        n = esc(&i.name),
        k = i.kind,
        s = linked(&i.signature, links, &i.name)
    );
    if i.docs.is_empty() {
        out.push_str("<p class=\"none\">No description.</p>\n");
    } else {
        out.push_str(&markdown(&i.docs));
    }
    if !i.members.is_empty() {
        out.push_str("<div class=\"members\">\n");
        for m in &i.members {
            item_html(out, m, links, "h3");
        }
        out.push_str("</div>\n");
    }
}
