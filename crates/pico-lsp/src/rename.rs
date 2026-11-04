//! Project-wide requests: rename (PLAN P10.6d2), every use of a declaration in the project's
//! modules, and implementations, the types and methods below a type or method.
//!
//! The project is the nearest directory up from the document with a `project.json` (else the
//! document's directory). Each of its `.pi` files is checked as an entry (open documents with
//! their unsaved text) and indexed; the uses whose declaration is the renamed one are edited.
//! Methods are renamed with their family (overrides, implementations and the interface
//! method). Shorthand struct literal fields (`S { x }`) are expanded (`S { y: x }`).

use crate::index::Index;
use crate::{check, path_to_uri, range, uri_to_path};
use pico_diag::Span;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

/// A declaration, independent of the check that found it: its file and name's offset.
type Decl = (PathBuf, u32);

/// The renameable name at `offset` of the document's index: its span and declaration.
pub fn target(idx: &Index, offset: u32) -> Result<(Span, Decl), String> {
    let r = idx.at(offset).ok_or("nothing to rename here")?;
    if r.builtin {
        return Err(format!("`{}` is built in and can't be renamed", r.name));
    }
    let at = Span::new(idx.file, r.start, r.end);
    Ok((at, decl_of(idx, r.def)?))
}

/// Where a declaration of `idx`'s program is, if it can be renamed (it's in the project, not
/// in the core or standard library).
fn decl_of(idx: &Index, def: Span) -> Result<Decl, String> {
    let name = &idx.sm.file(def.file).name;
    let path = Path::new(name);
    if !path.is_absolute() {
        return Err("declarations of the core library can't be renamed".into());
    }
    let path = canonical(path);
    if path.starts_with(canonical(&pico_driver::stdlib_dir())) {
        return Err("declarations of the standard library can't be renamed".into());
    }
    Ok((path, def.start))
}

fn canonical(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
}

/// The workspace edit renaming the declaration at `offset` of document `uri` to `new_name`.
/// `docs` are the open documents (by URI) and `idx` is `uri`'s index.
pub fn rename(
    docs: &HashMap<String, String>,
    uri: &str,
    idx: &Index,
    offset: u32,
    new_name: &str,
) -> Result<Value, String> {
    if !is_identifier(new_name) {
        return Err(format!("`{new_name}` isn't a name"));
    }
    if pico_syntax::token::Kw::parse(new_name).is_some() {
        return Err(format!("`{new_name}` is a keyword"));
    }
    let (_, target) = target(idx, offset)?;
    let indexes = project_indexes(docs, uri)?;
    // The declarations renamed together: the target, and its method family.
    let mut targets: HashSet<Decl> = HashSet::from([target.clone()]);
    for (_, i) in &indexes {
        for (span, family) in &i.families {
            if decl_of(i, *span).is_ok_and(|d| d == target) {
                for s in family {
                    let d = decl_of(i, *s).map_err(|_| {
                        "the method overrides or implements one of the core or standard library's"
                            .to_string()
                    })?;
                    targets.insert(d);
                }
            }
        }
    }
    let mut changes: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    let mut done: HashSet<(String, u32)> = HashSet::new();
    for (u, i) in &indexes {
        for &(at, def) in &i.links {
            if at.file != i.file || !decl_of(i, def).is_ok_and(|d| targets.contains(&d)) {
                continue;
            }
            if !done.insert((u.clone(), at.start)) {
                continue;
            }
            let old = i.sm.snippet(at);
            let text = if i.shorthand.contains(&at) {
                // `S { x }`: the field is renamed (`S { y: x }`) or the variable (`S { x: y }`).
                if field_decl(i, def) {
                    format!("{new_name}: {old}")
                } else {
                    format!("{old}: {new_name}")
                }
            } else {
                new_name.to_string()
            };
            changes
                .entry(u.clone())
                .or_default()
                .push(json!({ "range": range(&i.sm, at), "newText": text }));
        }
    }
    if changes.is_empty() {
        return Err("no uses of this name were found".into());
    }
    Ok(json!({ "changes": changes }))
}

/// Every file of the project around document `uri` checked as an entry and indexed (open
/// documents with their unsaved text), by URI.
pub fn project_indexes(
    docs: &HashMap<String, String>,
    uri: &str,
) -> Result<Vec<(String, Index)>, String> {
    let doc_path = uri_to_path(uri).ok_or("the document isn't a file")?;
    // Open documents by path, for their unsaved text and their URIs.
    let open: HashMap<PathBuf, (&String, &String)> = docs
        .iter()
        .filter_map(|(u, t)| Some((canonical(&uri_to_path(u)?), (u, t))))
        .collect();
    let mut files = pico_driver::project_files(&pico_driver::project_root(&doc_path));
    let doc_canon = canonical(&doc_path);
    if !files.iter().any(|f| canonical(f) == doc_canon) {
        files.push(doc_path.clone());
    }
    Ok(files
        .iter()
        .filter_map(|f| {
            let c = canonical(f);
            let (u, text) = match open.get(&c) {
                Some((u, t)) => ((*u).clone(), (*t).clone()),
                None => (path_to_uri(f), std::fs::read_to_string(f).ok()?),
            };
            let (_, index) = check(&u, &text);
            Some((u, index?))
        })
        .collect())
}

/// A declaration of any file (the core library's too), independent of the check: its file
/// (canonical path, or name for embedded files) and offset.
fn identity(i: &Index, def: Span) -> (String, u32) {
    let name = &i.sm.file(def.file).name;
    let p = Path::new(name);
    let file = if p.is_absolute() {
        canonical(p).display().to_string()
    } else {
        name.clone()
    };
    (file, def.start)
}

/// The implementations of the type or method named at `offset` of document `uri`: the types
/// below it, or the methods that implement or override it, in the whole project (`null` when
/// there are none).
pub fn implementations(
    docs: &HashMap<String, String>,
    uri: &str,
    idx: &Index,
    offset: u32,
) -> Result<Value, String> {
    let r = idx.at(offset).ok_or("nothing here")?;
    let target = identity(idx, r.def);
    // (The document's own index first: most answers are there.)
    let mut all = vec![(uri.to_string(), None)];
    all.extend(
        project_indexes(docs, uri)?
            .into_iter()
            .map(|(u, i)| (u, Some(i))),
    );
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for (_, i) in &all {
        let i = i.as_ref().unwrap_or(idx);
        for (decl, impls) in &i.implementations {
            if identity(i, *decl) != target {
                continue;
            }
            for s in impls {
                if seen.insert(identity(i, *s))
                    && let Some(u) = crate::file_uri(&i.sm, s.file)
                {
                    out.push(json!({ "uri": u, "range": range(&i.sm, *s) }));
                }
            }
        }
    }
    Ok(if out.is_empty() {
        Value::Null
    } else {
        json!(out)
    })
}

/// Whether `def` (in `i`'s program) declares a field (its hover says so).
fn field_decl(i: &Index, def: Span) -> bool {
    i.decls.get(&def).is_some_and(|h| h.contains("(field)"))
}

fn is_identifier(s: &str) -> bool {
    let mut cs = s.chars();
    cs.next()
        .is_some_and(|c| c.is_alphabetic() || c == '_' || c == '$')
        && cs.all(|c| c.is_alphanumeric() || c == '_' || c == '$')
}
