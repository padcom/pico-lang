//! Loading a program's modules (spec §8.1): the entry file and every file it imports,
//! transitively, in dependency order.

use ppl_diag::{Diagnostic, FileId, SourceMap, Span, codes};
use ppl_sema::{Import, Modules};
use ppl_syntax::ast::{self, DeclKind, StmtKind};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// The bundled standard library (`std/…` imports).
pub fn stdlib_dir() -> PathBuf {
    match std::env::var_os("PPL_STDLIB") {
        Some(d) => PathBuf::from(d),
        None => Path::new(env!("CARGO_MANIFEST_DIR")).join("../../stdlib"),
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
}

/// Loads the modules the entry (already parsed, in `dir`) imports.
pub fn load(sm: &mut SourceMap, entry: FileId, module: ast::Module, dir: &Path) -> Loaded {
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
    };
    l.modules.entry = Some(entry);
    if let Ok(p) = dir.join(file_name(l.sm, entry)).canonicalize() {
        l.by_path.insert(p, entry);
    }
    l.parsed.insert(entry, module);
    l.dirs.insert(entry, dir.to_path_buf());
    l.visit(entry);
    let mut stmts = Vec::new();
    for f in l.order.clone() {
        let m = l.parsed.remove(&f).expect("parsed");
        for st in m.stmts {
            match st.kind {
                StmtKind::Decl(d) => match d.kind {
                    DeclKind::Import(_)
                    | DeclKind::ExportList { .. }
                    | DeclKind::ExportAll { .. } => {}
                    DeclKind::Binding(inner) => stmts.push(*inner),
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
    Loaded {
        stmts,
        modules: l.modules,
        diags: l.diags,
    }
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
        let base = if spec.starts_with("./") || spec.starts_with("../") {
            self.dirs[&from].join(spec)
        } else if let Some(rest) = spec.strip_prefix("std/") {
            stdlib_dir().join(rest)
        } else {
            self.diags.push(
                Diagnostic::error(
                    codes::UNKNOWN_MODULE,
                    format!("cannot find module `{spec}`"),
                )
                .primary(span, "not a relative path or `std/…`")
                .note("packages from `project.json` aren't supported yet"),
            );
            return None;
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
        let candidates = [base.with_extension("ppl"), base.join("index.ppl")];
        let Some(path) = candidates.iter().find(|p| p.is_file()) else {
            self.diags.push(
                Diagnostic::error(
                    codes::UNKNOWN_MODULE,
                    format!("cannot find module `{spec}`"),
                )
                .primary(span, "no such file")
                .note(format!(
                    "looked for `{}` and `{}`",
                    candidates[0].display(),
                    candidates[1].display()
                )),
            );
            return None;
        };
        let canon = path.canonicalize().unwrap_or_else(|_| path.clone());
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
        let (m, mut d) = ppl_syntax::parse_module(f, &self.sm.file(f).text);
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

/// The string literals of `new Worker<…>("spec")` in a module's source, with the literal's
/// byte range.
fn worker_specs(text: &str) -> Vec<(String, u32, u32)> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(i) = text[from..].find("new Worker") {
        let mut at = from + i + "new Worker".len();
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
