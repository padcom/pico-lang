//! The ppl language server (`ppl lsp`, spec §14.1; PLAN P10.6b): JSON-RPC over stdio, as the
//! Language Server Protocol frames it (`Content-Length` headers).
//!
//! Documents are synced whole (`TextDocumentSyncKind.Full`). After a change the server waits
//! until edits pause ([`DEBOUNCE`]), runs the front end (`ppl_driver::check_source_with`) on
//! each changed document and publishes the diagnostics of every file they reach (imported
//! modules too). `textDocument/formatting` runs the formatter (P10.1) with the document's
//! `project.json` options. Hover and go-to-definition answer from the document's symbol
//! index ([`index`]), built by each check that gets as far as type checking (a document with
//! earlier errors keeps its last index).

mod index;
mod uri;

use ppl_diag::{Diagnostic, Severity, SourceMap};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;

pub use uri::{path_to_uri, uri_to_path};

/// How long edits must pause before a document is checked again.
pub const DEBOUNCE: Duration = Duration::from_millis(200);

/// Serves stdin/stdout until the client says `exit`.
pub fn run_stdio() -> std::io::Result<()> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut input = stdin.lock();
        while let Ok(Some(msg)) = read_message(&mut input) {
            if tx.send(msg).is_err() {
                break;
            }
        }
    });
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    serve(rx, |msg| {
        let _ = write_message(&mut out, &msg);
    });
    Ok(())
}

/// Reads one framed message (`None` at the end of the input).
pub fn read_message(input: &mut impl BufRead) -> std::io::Result<Option<Value>> {
    let mut len = None;
    loop {
        let mut line = String::new();
        if input.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':')
            && k.eq_ignore_ascii_case("content-length")
        {
            len = v.trim().parse::<usize>().ok();
        }
    }
    let Some(len) = len else {
        return Err(std::io::Error::other("a message without Content-Length"));
    };
    let mut body = vec![0; len];
    input.read_exact(&mut body)?;
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(std::io::Error::other)
}

/// Writes one framed message.
pub fn write_message(out: &mut impl Write, msg: &Value) -> std::io::Result<()> {
    let body = msg.to_string();
    write!(out, "Content-Length: {}\r\n\r\n{body}", body.len())?;
    out.flush()
}

/// The server's loop: messages from `rx`, everything it sends through `send`. Returns on
/// `exit`, or when `rx` closes.
pub fn serve(rx: Receiver<Value>, mut send: impl FnMut(Value)) {
    let mut s = Server::default();
    loop {
        let msg = if s.dirty.is_empty() {
            match rx.recv() {
                Ok(m) => m,
                Err(_) => return,
            }
        } else {
            match rx.recv_timeout(DEBOUNCE) {
                Ok(m) => m,
                Err(RecvTimeoutError::Timeout) => {
                    s.check_dirty(&mut send);
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => {
                    s.check_dirty(&mut send);
                    return;
                }
            }
        };
        if s.handle(msg, &mut send) {
            return;
        }
    }
}

#[derive(Default)]
struct Server {
    /// Open documents' text, by URI.
    docs: HashMap<String, String>,
    /// Documents changed since they were last checked.
    dirty: BTreeSet<String>,
    /// For each checked document, the URIs it published diagnostics for (cleared when they
    /// have none any more).
    published: HashMap<String, BTreeSet<String>>,
    /// The last symbol index of each open document.
    indexes: HashMap<String, index::Index>,
    shutdown: bool,
}

impl Server {
    /// Handles one message; `true` on `exit`.
    fn handle(&mut self, msg: Value, send: &mut impl FnMut(Value)) -> bool {
        let method = msg["method"].as_str().unwrap_or("").to_string();
        let id = msg.get("id").cloned();
        let params = &msg["params"];
        match (method.as_str(), id) {
            ("initialize", Some(id)) => send(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "capabilities": {
                        "textDocumentSync": { "openClose": true, "change": 1, "save": true },
                        "documentFormattingProvider": true,
                        "hoverProvider": true,
                        "definitionProvider": true,
                    },
                    "serverInfo": { "name": "ppl", "version": env!("CARGO_PKG_VERSION") },
                },
            })),
            ("shutdown", Some(id)) => {
                self.shutdown = true;
                send(json!({ "jsonrpc": "2.0", "id": id, "result": null }));
            }
            ("exit", _) => return true,
            ("textDocument/didOpen", None) => {
                let doc = &params["textDocument"];
                if let (Some(uri), Some(text)) = (doc["uri"].as_str(), doc["text"].as_str()) {
                    self.docs.insert(uri.to_string(), text.to_string());
                    self.dirty.insert(uri.to_string());
                }
            }
            ("textDocument/didChange", None) => {
                let uri = params["textDocument"]["uri"].as_str().unwrap_or("");
                // (Full sync: the last change is the whole text.)
                if let Some(text) = params["contentChanges"]
                    .as_array()
                    .and_then(|c| c.last())
                    .and_then(|c| c["text"].as_str())
                {
                    self.docs.insert(uri.to_string(), text.to_string());
                    self.dirty.insert(uri.to_string());
                }
            }
            ("textDocument/didSave", None) | ("workspace/didChangeWatchedFiles", None) => {
                // Other open documents may import the saved (or changed) module.
                self.dirty.extend(self.docs.keys().cloned());
            }
            ("textDocument/didClose", None) => {
                let uri = params["textDocument"]["uri"].as_str().unwrap_or("");
                self.docs.remove(uri);
                self.dirty.remove(uri);
                self.indexes.remove(uri);
                for u in self.published.remove(uri).unwrap_or_default() {
                    send(publish(&u, Vec::new()));
                }
            }
            ("textDocument/formatting", Some(id)) => {
                let uri = params["textDocument"]["uri"].as_str().unwrap_or("");
                let result = self.format(uri);
                send(json!({ "jsonrpc": "2.0", "id": id, "result": result }));
            }
            ("textDocument/hover" | "textDocument/definition", Some(id)) => {
                let uri = params["textDocument"]["uri"]
                    .as_str()
                    .unwrap_or("")
                    .to_string();
                if self.dirty.contains(&uri) {
                    self.check_dirty(send);
                }
                let result = match self.ref_at(&uri, &params["position"]) {
                    None => Value::Null,
                    Some((idx, r)) if method == "textDocument/hover" => json!({
                        "contents": { "kind": "markdown", "value": r.hover },
                        "range": {
                            "start": position(idx.sm.file(idx.file), r.start),
                            "end": position(idx.sm.file(idx.file), r.end),
                        },
                    }),
                    Some((idx, r)) => match file_uri(&idx.sm, r.def.file) {
                        Some(u) => json!({ "uri": u, "range": range(&idx.sm, r.def) }),
                        None => Value::Null,
                    },
                };
                send(json!({ "jsonrpc": "2.0", "id": id, "result": result }));
            }
            (_, Some(id)) if !method.is_empty() => send(json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {
                    "code": if self.shutdown { -32600 } else { -32601 },
                    "message": format!("unsupported request: {method}"),
                },
            })),
            // Notifications nobody handles, and responses to requests the server never sends.
            _ => {}
        }
        false
    }

    fn check_dirty(&mut self, send: &mut impl FnMut(Value)) {
        for uri in std::mem::take(&mut self.dirty) {
            let Some(text) = self.docs.get(&uri).cloned() else {
                continue;
            };
            let (by_uri, index) = check(&uri, &text);
            if let Some(i) = index {
                self.indexes.insert(uri.clone(), i);
            }
            let old = self.published.remove(&uri).unwrap_or_default();
            for u in old.difference(&by_uri.keys().cloned().collect()) {
                send(publish(u, Vec::new()));
            }
            let mut now = BTreeSet::new();
            for (u, diags) in by_uri {
                now.insert(u.clone());
                send(publish(&u, diags));
            }
            self.published.insert(uri, now);
        }
    }

    /// The indexed name at an LSP position of an open document. An index older than the
    /// text still answers where the name under the position is unchanged.
    fn ref_at(&self, uri: &str, pos: &Value) -> Option<(&index::Index, &index::Ref)> {
        let text = self.docs.get(uri)?;
        let idx = self.indexes.get(uri)?;
        let line = pos["line"].as_u64()? as usize;
        let character = pos["character"].as_u64()? as usize;
        let offset = offset_of(text, line, character)?;
        let r = idx.at(offset)?;
        if *text != idx.text && text.get(r.start as usize..r.end as usize) != Some(r.name.as_str())
        {
            return None;
        }
        Some((idx, r))
    }

    /// The document formatted, as one edit (`null` with syntax errors, as `ppl fmt`).
    fn format(&self, uri: &str) -> Value {
        let Some(text) = self.docs.get(uri) else {
            return Value::Null;
        };
        let (_, diags) = ppl_syntax::parse_module(ppl_diag::FileId(0), text);
        if diags.iter().any(Diagnostic::is_error) {
            return Value::Null;
        }
        let dir = uri_to_path(uri)
            .and_then(|p| p.parent().map(Path::to_path_buf))
            .unwrap_or_else(|| PathBuf::from("."));
        match ppl_syntax::fmt::format(text, ppl_driver::fmt_options(&dir)) {
            Ok(out) if out == *text => json!([]),
            Ok(out) => {
                let file = ppl_diag::SourceFile::new("", text.clone());
                let end = position(&file, text.len() as u32);
                json!([{
                    "range": { "start": { "line": 0, "character": 0 }, "end": end },
                    "newText": out,
                }])
            }
            Err(_) => Value::Null,
        }
    }
}

fn publish(uri: &str, diagnostics: Vec<Value>) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "textDocument/publishDiagnostics",
        "params": { "uri": uri, "diagnostics": diagnostics },
    })
}

/// Checks a document: its diagnostics and those of the files it imports, by URI (the
/// document's own entry is always there, possibly empty), and its symbol index if checking
/// got as far as types.
fn check(uri: &str, text: &str) -> (BTreeMap<String, Vec<Value>>, Option<index::Index>) {
    let path = uri_to_path(uri).unwrap_or_else(|| PathBuf::from("untitled.ppl"));
    let dir = path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let name = path.display().to_string();
    let mut out: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    out.insert(uri.to_string(), Vec::new());
    let checked = std::panic::catch_unwind(|| {
        ppl_driver::check_source_with(
            name.clone(),
            text.to_string(),
            &dir,
            ppl_driver::BuildOptions::default(),
        )
    });
    let mut checked = match checked {
        Ok(c) => c,
        Err(e) => {
            let why = e
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_default();
            out.get_mut(uri).expect("inserted").push(json!({
                "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 0 } },
                "severity": 1,
                "source": "ppl",
                "message": format!("internal compiler error: {why}"),
            }));
            return (out, None);
        }
    };
    for d in &checked.diags {
        let span = d.main_span();
        // (Diagnostics in the prelude, the stdlib or generated modules go on the document.)
        let target = span.and_then(|s| {
            let f = checked.sm.file(s.file);
            if f.name == name {
                return Some((uri.to_string(), Some(s)));
            }
            let p = Path::new(&f.name);
            (p.is_absolute() && p.exists()).then(|| (path_to_uri(p), Some(s)))
        });
        let (u, span) = target.unwrap_or((uri.to_string(), None));
        let v = to_lsp(d, span, &checked.sm, &name);
        out.entry(u).or_default().push(v);
    }
    let index = match (&checked.hir, &checked.types) {
        (Some(hir), Some(types)) => {
            let sm = std::mem::take(&mut checked.sm);
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                index::build(sm, hir, types)
            }))
            .ok()
        }
        _ => None,
    };
    (out, index)
}

/// A URI the client can open for a source file: its path, or for the embedded core library
/// (`<core>/…`) and generated modules (`<c:…>`, `<js:…>`) the bundled stdlib's copy or a
/// read-only copy in the temporary directory.
fn file_uri(sm: &SourceMap, file: ppl_diag::FileId) -> Option<String> {
    let f = sm.file(file);
    let p = Path::new(&f.name);
    if p.is_absolute() {
        return Some(path_to_uri(p));
    }
    if let Some(rest) = f.name.strip_prefix("<core>/") {
        let bundled = ppl_driver::stdlib_dir().join("core").join(rest);
        if std::fs::read_to_string(&bundled).is_ok_and(|t| t == f.text) {
            return Some(path_to_uri(&bundled.canonicalize().unwrap_or(bundled)));
        }
    }
    let name: String = f
        .name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let dir = std::env::temp_dir().join(format!("ppl-lsp-{}", env!("CARGO_PKG_VERSION")));
    let path = dir.join(if name.ends_with(".ppl") {
        name
    } else {
        format!("{name}.ppl")
    });
    if std::fs::read_to_string(&path).ok().as_deref() != Some(f.text.as_str()) {
        std::fs::create_dir_all(&dir).ok()?;
        // (An older copy is read-only.)
        if let Ok(m) = std::fs::metadata(&path) {
            let mut perm = m.permissions();
            #[allow(clippy::permissions_set_readonly_false)]
            perm.set_readonly(false);
            let _ = std::fs::set_permissions(&path, perm);
        }
        std::fs::write(&path, &f.text).ok()?;
        let mut perm = std::fs::metadata(&path).ok()?.permissions();
        perm.set_readonly(true);
        let _ = std::fs::set_permissions(&path, perm);
    }
    Some(path_to_uri(&path))
}

/// The byte offset of an LSP position (UTF-16 code units in the line).
fn offset_of(text: &str, line: usize, character: usize) -> Option<u32> {
    let mut start = 0;
    for _ in 0..line {
        start += text[start..].find('\n')? + 1;
    }
    let rest = &text[start..];
    let end = rest.find('\n').unwrap_or(rest.len());
    let mut units = 0;
    for (i, c) in rest[..end].char_indices() {
        if units >= character {
            return Some((start + i) as u32);
        }
        units += c.len_utf16();
    }
    Some((start + end) as u32)
}

/// A diagnostic as LSP sends it (at `span`, or the document's start).
fn to_lsp(d: &Diagnostic, span: Option<ppl_diag::Span>, sm: &SourceMap, entry: &str) -> Value {
    let at = match span {
        Some(s) => range(sm, s),
        None => {
            json!({ "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 0 } })
        }
    };
    let mut message = d.message.clone();
    if span.is_none()
        && let Some(s) = d.main_span()
    {
        let f = sm.file(s.file);
        let (l, c) = f.line_col(s.start);
        message.push_str(&format!("\n  at {}:{}:{}", f.name, l + 1, c + 1));
    }
    for n in &d.notes {
        message.push_str(&format!("\nnote: {n}"));
    }
    for h in &d.help {
        message.push_str(&format!("\nhelp: {h}"));
    }
    let primary = d.labels.iter().find(|l| l.primary);
    if let Some(l) = primary
        && !l.message.is_empty()
        && l.message != d.message
    {
        message.push_str(&format!("\n{}", l.message));
    }
    let related: Vec<Value> = d
        .labels
        .iter()
        .filter(|l| !l.primary && !l.message.is_empty())
        .filter_map(|l| {
            let f = sm.file(l.span.file);
            let uri = if f.name == entry {
                path_to_uri(Path::new(entry))
            } else {
                let p = Path::new(&f.name);
                if !p.is_absolute() {
                    return None;
                }
                path_to_uri(p)
            };
            Some(json!({
                "location": { "uri": uri, "range": range(sm, l.span) },
                "message": l.message,
            }))
        })
        .collect();
    let mut v = json!({
        "range": at,
        "severity": match d.severity {
            Severity::Error => 1,
            Severity::Warning => 2,
            Severity::Note => 3,
        },
        "source": "ppl",
        "message": message,
    });
    if let Some(c) = d.code {
        v["code"] = json!(c.to_string());
    }
    if !related.is_empty() {
        v["relatedInformation"] = json!(related);
    }
    v
}

fn range(sm: &SourceMap, s: ppl_diag::Span) -> Value {
    let f = sm.file(s.file);
    json!({ "start": position(f, s.start), "end": position(f, s.end) })
}

/// A byte offset as an LSP position (UTF-16 code units in the line).
fn position(f: &ppl_diag::SourceFile, offset: u32) -> Value {
    let offset = offset.min(f.text.len() as u32);
    let line = f.line_index(offset);
    let start = f.line_start(line) as usize;
    let col: usize = f.text[start..offset as usize]
        .chars()
        .map(char::len_utf16)
        .sum();
    json!({ "line": line, "character": col })
}

#[cfg(test)]
mod tests;
