//! Loading a program's modules (spec §8.1): the entry file and every file it imports,
//! transitively, in dependency order.

use pico_diag::{Diagnostic, FileId, SourceMap, Span, codes};
use pico_sema::{Import, Modules};
use pico_syntax::ast::{self, DeclKind, StmtKind};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

/// The bundled standard library (`std/…` imports).
pub fn stdlib_dir() -> PathBuf {
    match std::env::var_os("PICO_STDLIB") {
        Some(d) => PathBuf::from(d),
        None => pico_runtime::installed_stdlib()
            .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../stdlib")),
    }
}

pub struct Loaded {
    /// Every module's statements, dependencies first, imports and export lists removed.
    pub stmts: Vec<ast::Stmt>,
    pub modules: Modules,
    pub diags: Vec<Diagnostic>,
}

struct Loader<'s> {
    sm: &'s mut SourceMap,
    by_path: HashMap<PathBuf, FileId>,
    parsed: HashMap<FileId, ast::Module>,
    dirs: HashMap<FileId, PathBuf>,
    visiting: HashSet<FileId>,
    done: HashSet<FileId>,
    order: Vec<FileId>,
    modules: Modules,
    diags: Vec<Diagnostic>,
    /// Names imported from each `js:` module (spec §12.3): collected by a first pass, which
    /// skips those modules; the second generates each one for all of them (decision 259).
    js_requests: BTreeMap<String, BTreeSet<String>>,
    js_names: Option<BTreeMap<String, BTreeSet<String>>>,
    /// The target's `target.os`.
    os: String,
    /// When the entry is a test (`*.test.pi`, `*.bench.pi`): its package's directory, whose
    /// files see its `devDependencies` (spec §8.3).
    dev_root: Option<PathBuf>,
    /// The build's packages (git dependencies' checkouts, from `project.lock`), prepared once.
    prepared: Option<Result<Option<crate::packages::Prepared>, String>>,
}

/// Loads the modules the entry (already parsed, in `dir`) imports.
/// `os` is the target's (`target.os`): it picks platform files (spec §15.4) and decides what
/// is available (spec §13.3).
pub fn load(
    sm: &mut SourceMap,
    entry: FileId,
    module: ast::Module,
    dir: &Path,
    os: &str,
) -> Loaded {
    let first = load_pass(sm, entry, module.clone(), dir, os, None);
    match first {
        Ok(loaded) => loaded,
        // `js:` imports: again, generating those modules.
        Err(requests) => match load_pass(sm, entry, module, dir, os, Some(requests)) {
            Ok(loaded) => loaded,
            Err(_) => unreachable!("the second pass generates js: modules"),
        },
    }
}

fn load_pass(
    sm: &mut SourceMap,
    entry: FileId,
    module: ast::Module,
    dir: &Path,
    os: &str,
    js_names: Option<BTreeMap<String, BTreeSet<String>>>,
) -> Result<Loaded, BTreeMap<String, BTreeSet<String>>> {
    let mut l = Loader {
        sm,
        by_path: HashMap::new(),
        parsed: HashMap::new(),
        dirs: HashMap::new(),
        visiting: HashSet::new(),
        done: HashSet::new(),
        order: Vec::new(),
        modules: Modules::default(),
        diags: Vec::new(),
        js_requests: BTreeMap::new(),
        js_names,
        os: os.to_string(),
        dev_root: None,
        prepared: None,
    };
    let name = file_name(l.sm, entry);
    if name.ends_with(".test.pi") || name.ends_with(".bench.pi") {
        l.dev_root = crate::packages::package_of(dir)
            .and_then(|m| m.ok())
            .map(|m| m.dir);
    }
    l.modules.entry = Some(entry);
    if let Ok(p) = pico_runtime::canonical(&dir.join(file_name(l.sm, entry))) {
        l.by_path.insert(p, entry);
    }
    l.parsed.insert(entry, module);
    l.dirs.insert(entry, dir.to_path_buf());
    l.visit(entry);
    if l.js_names.is_none() && !l.js_requests.is_empty() {
        return Err(l.js_requests);
    }
    let mut stmts = Vec::new();
    for f in l.order.clone() {
        let m = l.parsed.remove(&f).expect("parsed");
        for st in m.stmts {
            match st.kind {
                StmtKind::Decl(d) => match d.kind {
                    DeclKind::Import(_)
                    | DeclKind::ExportList { .. }
                    | DeclKind::ExportAll { .. } => {}
                    DeclKind::Binding(inner) => {
                        if !d.decorators.is_empty() {
                            l.modules
                                .binding_decorators
                                .insert(inner.span, d.decorators);
                        }
                        stmts.push(*inner)
                    }
                    kind => stmts.push(ast::Stmt {
                        kind: StmtKind::Decl(ast::Decl { kind, ..d }),
                        span: st.span,
                    }),
                },
                kind => stmts.push(ast::Stmt {
                    kind,
                    span: st.span,
                }),
            }
        }
    }
    Ok(Loaded {
        stmts,
        modules: l.modules,
        diags: l.diags,
    })
}

fn file_name(sm: &SourceMap, f: FileId) -> String {
    let name = &sm.file(f).name;
    Path::new(name)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| name.clone())
}

/// The names a declaration exports.
fn declared_names(d: &ast::Decl) -> Vec<String> {
    let one = |i: &ast::Ident| vec![i.name.clone()];
    match &d.kind {
        DeclKind::Function(f) => f.name.as_ref().map(one).unwrap_or_default(),
        DeclKind::Struct(s) => one(&s.name),
        DeclKind::Enum(e) => one(&e.name),
        DeclKind::Class(c) => one(&c.name),
        DeclKind::Interface(i) => one(&i.name),
        DeclKind::TypeAlias { name, .. } => one(name),
        DeclKind::Extern { items, .. } => items.iter().flat_map(declared_names).collect(),
        DeclKind::Binding(st) => match &st.kind {
            StmtKind::Let { pat, .. } => {
                let mut out = Vec::new();
                pattern_names(pat, &mut out);
                out
            }
            _ => Vec::new(),
        },
        _ => Vec::new(),
    }
}

fn pattern_names(p: &ast::Pattern, out: &mut Vec<String>) {
    match &p.kind {
        ast::PatternKind::Ident(i) => out.push(i.name.clone()),
        ast::PatternKind::Array(elems) => {
            for e in elems {
                pattern_names(e, out);
            }
        }
        _ => {}
    }
}

impl Loader<'_> {
    fn visit(&mut self, f: FileId) {
        self.visiting.insert(f);
        // Imports and exports, in order.
        let stmts: Vec<ast::Stmt> = self.parsed[&f].stmts.clone();
        let mut exports: HashMap<String, String> = HashMap::new();
        for st in &stmts {
            let StmtKind::Decl(d) = &st.kind else {
                continue;
            };
            if d.exported {
                for n in declared_names(d) {
                    exports.insert(n.clone(), n);
                }
            }
            match &d.kind {
                DeclKind::Import(imp) => {
                    if self.unavailable(f, imp) {
                        continue;
                    }
                    if let Some(js) = imp.from.strip_prefix("js:") {
                        match &imp.kind {
                            ast::ImportKind::Named(names) => {
                                let set = self.js_requests.entry(js.to_string()).or_default();
                                set.extend(names.iter().map(|n| n.name.name.clone()));
                            }
                            ast::ImportKind::Namespace(_) => {
                                self.diags.push(
                                    Diagnostic::error(
                                        codes::UNKNOWN_MODULE,
                                        format!("`{}` needs named imports", imp.from),
                                    )
                                    .primary(imp.from_span, "imported as a namespace")
                                    .note("a `js:` module is generated for the names it's asked for: `import { document } from \"js:dom\"`"),
                                );
                                continue;
                            }
                        }
                    }
                    let Some(t) = self.module(f, &imp.from, imp.from_span) else {
                        continue;
                    };
                    match &imp.kind {
                        ast::ImportKind::Named(names) => {
                            for n in names {
                                self.modules.imports.push(Import {
                                    module: f,
                                    target: t,
                                    name: Some(n.name.clone()),
                                    alias: n.alias.clone().unwrap_or_else(|| n.name.clone()),
                                });
                            }
                        }
                        ast::ImportKind::Namespace(alias) => self.modules.imports.push(Import {
                            module: f,
                            target: t,
                            name: None,
                            alias: alias.clone(),
                        }),
                    }
                }
                DeclKind::ExportList { names, from } => match from {
                    // `export { a as b }`
                    None => {
                        for n in names {
                            let alias = n.alias.as_ref().unwrap_or(&n.name);
                            exports.insert(alias.name.clone(), n.name.name.clone());
                        }
                    }
                    // `export { a } from "./x"`: imported and exported.
                    Some(spec) => {
                        let Some(t) = self.module(f, spec, d.span) else {
                            continue;
                        };
                        for n in names {
                            let alias = n.alias.clone().unwrap_or_else(|| n.name.clone());
                            exports.insert(alias.name.clone(), alias.name.clone());
                            self.modules.imports.push(Import {
                                module: f,
                                target: t,
                                name: Some(n.name.clone()),
                                alias,
                            });
                        }
                    }
                },
                DeclKind::ExportAll { from } => {
                    let Some(t) = self.module(f, from, d.span) else {
                        continue;
                    };
                    let theirs: Vec<String> = self
                        .modules
                        .exports
                        .get(&t)
                        .map(|e| e.keys().cloned().collect())
                        .unwrap_or_default();
                    for n in theirs {
                        exports.insert(n.clone(), n.clone());
                        let ident = ast::Ident {
                            name: n,
                            span: d.span,
                        };
                        self.modules.imports.push(Import {
                            module: f,
                            target: t,
                            name: Some(ident.clone()),
                            alias: ident,
                        });
                    }
                }
                _ => {}
            }
        }
        // Worker modules (`new Worker<…>("./job")`, spec §10.2) are loaded as roots of their
        // own: found in the source text (the resolver matches the literal again).
        let text = self.sm.file(f).text.clone();
        for (spec, start, end) in worker_specs(&text) {
            let span = Span::new(f, start, end);
            if let Some(t) = self.module(f, &spec, span) {
                self.modules.workers.push((f, spec, t));
            }
        }
        self.modules.exports.insert(f, exports);
        self.visiting.remove(&f);
        self.done.insert(f);
        self.order.push(f);
    }

    /// The module a specifier in file `from` names, loading (and visiting) it first.
    fn module(&mut self, from: FileId, spec: &str, span: Span) -> Option<FileId> {
        if let Some(js) = spec.strip_prefix("js:") {
            return self.js_module(from, js, span);
        }
        if let Some(header) = spec.strip_prefix("c:") {
            return self.c_module(from, header, span);
        }
        let base = if spec.starts_with("./") || spec.starts_with("../") {
            self.dirs[&from].join(spec)
        } else if let Some(rest) = spec.strip_prefix("std/") {
            stdlib_dir().join(rest)
        } else {
            // A package (spec §8.3): the dependency's root `index.pi`.
            if self.prepared.is_none() {
                let entry_dir = self.dirs[&self.modules.entry.expect("entry")].clone();
                self.prepared = Some(crate::packages::prepare(&entry_dir));
            }
            match crate::packages::resolve(
                spec,
                &self.dirs[&from],
                self.dev_root.as_deref(),
                self.prepared.as_ref(),
            ) {
                Ok(index) => index,
                Err(why) => {
                    self.diags.push(
                        Diagnostic::error(codes::UNKNOWN_MODULE, format!("cannot import `{spec}`"))
                            .primary(span, "not found")
                            .note(why),
                    );
                    return None;
                }
            }
        };
        // `a/./b/../c` → `a/c`, for readable names in messages.
        let mut clean = PathBuf::new();
        for c in base.components() {
            match c {
                std::path::Component::CurDir => {}
                std::path::Component::ParentDir if clean.file_name().is_some() => {
                    clean.pop();
                }
                c => clean.push(c),
            }
        }
        let base = clean;
        // Platform files first (spec §15.4): `x.linux.pi`, then `x.wasm.pi` on wasm targets.
        let file = base
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let mut candidates = vec![base.with_file_name(format!("{file}.{}.pi", self.os))];
        if self.os == "wasi" || self.os == "browser" {
            candidates.push(base.with_file_name(format!("{file}.wasm.pi")));
        }
        candidates.push(base.with_file_name(format!("{file}.pi")));
        candidates.push(base.join("index.pi"));
        let Some(path) = candidates.iter().find(|p| p.is_file()) else {
            self.diags.push(
                Diagnostic::error(
                    codes::UNKNOWN_MODULE,
                    format!("cannot find module `{spec}`"),
                )
                .primary(span, "no such file")
                .note(format!(
                    "looked for {}",
                    candidates
                        .iter()
                        .map(|c| format!("`{}`", c.display()))
                        .collect::<Vec<_>>()
                        .join(", ")
                )),
            );
            return None;
        };
        let canon = pico_runtime::canonical(path).unwrap_or_else(|_| path.clone());
        if let Some(&f) = self.by_path.get(&canon) {
            // Already loaded (or being loaded: an import cycle, which is fine for declarations).
            return Some(f);
        }
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) => {
                self.diags.push(
                    Diagnostic::error(
                        codes::UNKNOWN_MODULE,
                        format!("cannot read module `{spec}`"),
                    )
                    .primary(span, e.to_string()),
                );
                return None;
            }
        };
        let name = path.display().to_string();
        let f = self.sm.add(name, text);
        self.by_path.insert(canon, f);
        let (m, mut d) = pico_syntax::parse_module(f, &self.sm.file(f).text);
        self.diags.append(&mut d);
        self.parsed.insert(f, m);
        self.dirs
            .insert(f, path.parent().map(Path::to_path_buf).unwrap_or_default());
        self.modules.names.insert(f, spec.to_string());
        if !self.done.contains(&f) && !self.visiting.contains(&f) {
            self.visit(f);
        }
        Some(f)
    }
}

/// The target's name in messages.
fn target_name(os: &str) -> &'static str {
    match os {
        "browser" => "wasm32-browser",
        "wasi" => "wasm32-wasi",
        "linux" => "Linux",
        "macos" => "macOS",
        _ => "Windows",
    }
}

/// What a target doesn't have (spec §13.3): a whole module, or one of its names.
fn unavailable_on(os: &str, spec: &str, name: Option<&str>) -> bool {
    let browser = os == "browser";
    match (spec, name) {
        ("std/process", Some("Command")) => browser || os == "wasi",
        ("std/fs" | "std/net" | "std/process", _) => browser,
        ("std/http", Some("serve" | "HttpServer" | "upgradeWebSocket")) => browser,
        ("std/js", _) => !browser,
        _ => spec.starts_with("js:") && !browser,
    }
}

impl Loader<'_> {
    /// Reports an import of something the target doesn't have (spec §13.3), from the program's
    /// own code (the standard library sorts out its own imports). True when the whole module
    /// is unavailable (the import is then skipped).
    fn unavailable(&mut self, f: FileId, imp: &ast::ImportDecl) -> bool {
        // (A module's name is the specifier it was first loaded by.)
        let lib =
            self.modules.names.get(&f).is_some_and(|n| {
                n.starts_with("std/") || n.starts_with("js:") || n.starts_with("c:")
            });
        if lib {
            return false;
        }
        let target = target_name(&self.os);
        let fix =
            "use a platform file (`x.wasm.pi`, `x.linux.pi`, …) or `comptime if (target.os == …)`";
        if unavailable_on(&self.os, &imp.from, None) {
            self.diags.push(
                Diagnostic::error(
                    codes::UNAVAILABLE,
                    format!("`{}` isn't available on {target}", imp.from),
                )
                .primary(imp.from_span, format!("not on {target}"))
                .note(fix),
            );
            return true;
        }
        if let ast::ImportKind::Named(names) = &imp.kind {
            for n in names {
                if unavailable_on(&self.os, &imp.from, Some(&n.name.name)) {
                    self.diags.push(
                        Diagnostic::error(
                            codes::UNAVAILABLE,
                            format!(
                                "`{}` from `{}` isn't available on {target}",
                                n.name.name, imp.from
                            ),
                        )
                        .primary(n.name.span, format!("not on {target}"))
                        .note(fix),
                    );
                }
            }
        }
        false
    }

    /// A `c:` module (spec §12.2): generated from the C header through clang, with the
    /// include and link settings of `project.json` (decision 262).
    fn c_module(&mut self, from: FileId, header: &str, span: Span) -> Option<FileId> {
        let dir = self.dirs[&from].clone();
        let key = if header.starts_with("./") || header.starts_with("../") {
            PathBuf::from(format!("<c:{}>", dir.join(header).display()))
        } else {
            PathBuf::from(format!("<c:{header}>"))
        };
        if let Some(&f) = self.by_path.get(&key) {
            return Some(f);
        }
        let entry_dir = self
            .modules
            .entry
            .and_then(|e| self.dirs.get(&e))
            .cloned()
            .unwrap_or_default();
        let text = crate::cheader::config(&entry_dir)
            .and_then(|cfg| crate::cheader::generate(header, &dir, &cfg));
        let text = match text {
            Ok(t) => t,
            Err(why) => {
                self.diags.push(
                    Diagnostic::error(codes::UNKNOWN_MODULE, format!("cannot import `c:{header}`"))
                        .primary(span, "this C header")
                        .note(why),
                );
                return None;
            }
        };
        let f = self.sm.add(format!("<c:{header}>"), text);
        self.by_path.insert(key, f);
        let (m, mut d) = pico_syntax::parse_module(f, &self.sm.file(f).text);
        self.diags.append(&mut d);
        self.parsed.insert(f, m);
        self.dirs.insert(f, dir);
        self.modules.names.insert(f, format!("c:{header}"));
        self.visit(f);
        Some(f)
    }

    /// A `js:` module (spec §12.3): generated from TypeScript declarations for every name the
    /// program imports from it (none in the first pass).
    fn js_module(&mut self, from: FileId, js: &str, span: Span) -> Option<FileId> {
        let names = self.js_names.as_ref()?.get(js)?.clone();
        let key = PathBuf::from(format!("<js:{js}>"));
        if let Some(&f) = self.by_path.get(&key) {
            return Some(f);
        }
        let dts = match js_declarations(js, &self.dirs[&from]) {
            Ok(d) => d,
            Err(why) => {
                self.diags.push(
                    Diagnostic::error(codes::UNKNOWN_MODULE, format!("cannot find module `js:{js}`"))
                        .primary(span, why)
                        .note("`js:dom` is the browser's API; `js:npm/<package>` a package in `node_modules` with TypeScript declarations"),
                );
                return None;
            }
        };
        let decls = crate::dts::parse(&dts);
        let missing: Vec<&String> = names
            .iter()
            .filter(|n| {
                !decls.vars.contains_key(*n)
                    && !decls.fns.contains_key(*n)
                    && !decls.ifaces.contains_key(*n)
            })
            .collect();
        if let Some(n) = missing.first() {
            self.diags.push(
                Diagnostic::error(
                    codes::NOT_EXPORTED,
                    format!("`js:{js}` doesn't declare `{n}`"),
                )
                .primary(span, "imported from here"),
            );
        }
        let text = crate::dts::generate(&decls, js, &names);
        let f = self.sm.add(format!("<js:{js}>"), text);
        self.by_path.insert(key, f);
        let (m, mut d) = pico_syntax::parse_module(f, &self.sm.file(f).text);
        self.diags.append(&mut d);
        self.parsed.insert(f, m);
        self.dirs.insert(f, stdlib_dir());
        self.modules.names.insert(f, format!("js:{js}"));
        self.visit(f);
        Some(f)
    }
}

/// The TypeScript declarations of `js:<js>`: TypeScript's `lib.dom.d.ts` for `dom`, or a
/// package's in the nearest `node_modules` for `npm/<package>`.
fn js_declarations(js: &str, dir: &Path) -> Result<String, String> {
    let path = if js == "dom" {
        pico_runtime::typescript_dir().join("lib.dom.d.ts")
    } else if let Some(pkg) = js.strip_prefix("npm/") {
        let mut found = None;
        let mut d = Some(pico_runtime::canonical(dir).unwrap_or_else(|_| dir.to_path_buf()));
        while let Some(cur) = d {
            let p = cur.join("node_modules").join(pkg);
            if p.is_dir() {
                found = Some(p);
                break;
            }
            d = cur.parent().map(Path::to_path_buf);
        }
        let p = found.ok_or_else(|| format!("no `node_modules/{pkg}` here or above"))?;
        let manifest = std::fs::read_to_string(p.join("package.json")).unwrap_or_default();
        let types = ["\"types\"", "\"typings\""].iter().find_map(|k| {
            let at = manifest.find(k)?;
            let rest = &manifest[at + k.len()..];
            let start = rest.find('"')? + 1;
            let end = rest[start..].find('"')?;
            Some(rest[start..start + end].to_string())
        });
        p.join(types.unwrap_or_else(|| "index.d.ts".into()))
    } else {
        return Err(format!("unknown `js:` module `{js}`"));
    };
    std::fs::read_to_string(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))
}

/// The string literals of `new Worker<…>("spec")` in a module's source, with the literal's
/// byte range.
fn worker_specs(text: &str) -> Vec<(String, u32, u32)> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(i) = text[from..].find("new Worker") {
        let start_of = from + i;
        let mut at = start_of + "new Worker".len();
        // (Not in a `//` comment.)
        let line_start = text[..start_of].rfind('\n').map_or(0, |n| n + 1);
        if text[line_start..start_of].contains("//") {
            from = at;
            continue;
        }
        if text[at..].starts_with("Pool") {
            at += "Pool".len();
        }
        from = at;
        let skip_ws = |at: &mut usize| {
            while *at < b.len() && (b[*at] as char).is_whitespace() {
                *at += 1;
            }
        };
        skip_ws(&mut at);
        if at < b.len() && b[at] == b'<' {
            // Type arguments (nested `<…>` too).
            let mut depth = 0;
            while at < b.len() {
                match b[at] {
                    b'<' => depth += 1,
                    b'>' => {
                        depth -= 1;
                        if depth == 0 {
                            at += 1;
                            break;
                        }
                    }
                    _ => {}
                }
                at += 1;
            }
            skip_ws(&mut at);
        }
        if at >= b.len() || b[at] != b'(' {
            continue;
        }
        at += 1;
        skip_ws(&mut at);
        if at >= b.len() || b[at] != b'"' {
            continue;
        }
        let start = at;
        let Some(len) = text[at + 1..].find('"') else {
            continue;
        };
        let spec = text[at + 1..at + 1 + len].to_string();
        out.push((spec, start as u32, (at + 2 + len) as u32));
    }
    out
}
