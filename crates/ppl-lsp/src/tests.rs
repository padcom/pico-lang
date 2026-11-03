//! Protocol tests: scripted sessions through `serve`.

use super::*;
use std::sync::mpsc;

/// Runs a session: `msgs` in order, then waits for checks to finish and ends the input.
fn session(msgs: Vec<Value>) -> Vec<Value> {
    let (tx, rx) = mpsc::channel();
    for m in msgs {
        tx.send(m).unwrap();
    }
    drop(tx);
    let mut out = Vec::new();
    serve(rx, |m| out.push(m));
    out
}

fn init() -> Value {
    json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "capabilities": {} } })
}

fn open(uri: &str, text: &str) -> Value {
    json!({ "jsonrpc": "2.0", "method": "textDocument/didOpen", "params": {
        "textDocument": { "uri": uri, "languageId": "ppl", "version": 1, "text": text } } })
}

fn diags_for<'a>(out: &'a [Value], uri: &str) -> Vec<&'a Value> {
    out.iter()
        .filter(|m| m["method"] == "textDocument/publishDiagnostics" && m["params"]["uri"] == uri)
        .collect()
}

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("ppl-lsp-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn initialize_and_shutdown() {
    let out = session(vec![
        init(),
        json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
        json!({ "jsonrpc": "2.0", "id": 2, "method": "textDocument/hover", "params": {} }),
        json!({ "jsonrpc": "2.0", "id": 3, "method": "shutdown" }),
        json!({ "jsonrpc": "2.0", "method": "exit" }),
    ]);
    assert_eq!(out.len(), 3, "{out:#?}");
    assert_eq!(out[0]["id"], 1);
    let caps = &out[0]["result"]["capabilities"];
    assert_eq!(caps["textDocumentSync"]["change"], 1);
    assert_eq!(caps["documentFormattingProvider"], true);
    assert_eq!(out[1]["error"]["code"], -32601);
    assert_eq!(out[2]["id"], 3);
    assert!(out[2]["result"].is_null());
}

#[test]
fn diagnostics_on_open_and_change() {
    let dir = tmp("diag");
    let uri = path_to_uri(&dir.join("main.ppl"));
    let out = session(vec![
        init(),
        open(&uri, "const x: int = \"no\"\nprintln(x)\n"),
    ]);
    let d = diags_for(&out, &uri);
    assert_eq!(d.len(), 1, "{out:#?}");
    let list = d[0]["params"]["diagnostics"].as_array().unwrap();
    assert_eq!(list.len(), 1, "{list:#?}");
    assert_eq!(list[0]["severity"], 1);
    assert!(list[0]["code"].as_str().unwrap().starts_with('E'));
    assert_eq!(list[0]["range"]["start"]["line"], 0);

    // Fixed by a change: the diagnostics are cleared (one check for both messages).
    let out = session(vec![
        init(),
        open(&uri, "const x: int = \"no\"\n"),
        json!({ "jsonrpc": "2.0", "method": "textDocument/didChange", "params": {
            "textDocument": { "uri": uri, "version": 2 },
            "contentChanges": [{ "text": "const x: int = 1\nprintln(x)\n" }] } }),
    ]);
    let d = diags_for(&out, &uri);
    assert_eq!(d.len(), 1, "{out:#?}");
    assert_eq!(d[0]["params"]["diagnostics"], json!([]));
}

#[test]
fn utf16_positions() {
    let dir = tmp("utf16");
    let uri = path_to_uri(&dir.join("main.ppl"));
    // (`🙂` is two UTF-16 code units, four bytes.)
    let out = session(vec![
        init(),
        open(&uri, "const s = \"🙂\"; const y: int = s\n"),
    ]);
    let d = diags_for(&out, &uri);
    let list = d[0]["params"]["diagnostics"].as_array().unwrap();
    assert_eq!(list.len(), 1, "{list:#?}");
    let col = list[0]["range"]["start"]["character"].as_u64().unwrap();
    let text = "const s = \"🙂\"; const y: int = s";
    let byte = text.rfind('s').unwrap();
    let want: usize = text[..byte].chars().map(char::len_utf16).sum();
    assert!(col as usize <= want, "{col} vs {want}: {list:#?}");
}

#[test]
fn diagnostics_in_imported_modules() {
    let dir = tmp("import");
    std::fs::write(dir.join("lib.ppl"), "export function f(): int { \"x\" }\n").unwrap();
    let uri = path_to_uri(&dir.join("main.ppl"));
    let lib = path_to_uri(&dir.join("lib.ppl"));
    let out = session(vec![
        init(),
        open(&uri, "import { f } from \"./lib\"\nprintln(f())\n"),
    ]);
    let d = diags_for(&out, &lib);
    assert_eq!(d.len(), 1, "{out:#?}");
    assert!(!d[0]["params"]["diagnostics"].as_array().unwrap().is_empty());
}

#[test]
fn formatting() {
    let dir = tmp("fmt");
    let uri = path_to_uri(&dir.join("main.ppl"));
    let fmt = |id: i64| {
        json!({ "jsonrpc": "2.0", "id": id, "method": "textDocument/formatting", "params": {
            "textDocument": { "uri": uri }, "options": { "tabSize": 2, "insertSpaces": true } } })
    };
    let out = session(vec![
        init(),
        open(&uri, "function f( ) {\nreturn 1\n}\n"),
        fmt(2),
    ]);
    let r = out.iter().find(|m| m["id"] == 2).unwrap();
    let edits = r["result"].as_array().unwrap();
    assert_eq!(edits.len(), 1);
    assert_eq!(edits[0]["newText"], "function f() {\n  return 1\n}\n");
    assert_eq!(edits[0]["range"]["end"]["line"], 3);
    // Syntax errors: nothing.
    let out = session(vec![init(), open(&uri, "function f( {\n"), fmt(3)]);
    let r = out.iter().find(|m| m["id"] == 3).unwrap();
    assert!(r["result"].is_null());
}

#[test]
fn close_clears() {
    let dir = tmp("close");
    let uri = path_to_uri(&dir.join("main.ppl"));
    let (tx, rx) = mpsc::channel();
    let t = std::thread::spawn(move || {
        let mut out = Vec::new();
        serve(rx, |m| out.push(m));
        out
    });
    tx.send(init()).unwrap();
    tx.send(open(&uri, "const x: int = \"no\"\n")).unwrap();
    std::thread::sleep(DEBOUNCE * 3);
    tx.send(
        json!({ "jsonrpc": "2.0", "method": "textDocument/didClose", "params": {
        "textDocument": { "uri": uri } } }),
    )
    .unwrap();
    drop(tx);
    let out = t.join().unwrap();
    let d = diags_for(&out, &uri);
    assert_eq!(d.len(), 2, "{out:#?}");
    assert_eq!(d[1]["params"]["diagnostics"], json!([]));
}

#[test]
fn framing() {
    let msg = json!({ "jsonrpc": "2.0", "id": 1, "method": "x", "params": { "s": "é" } });
    let mut buf = Vec::new();
    write_message(&mut buf, &msg).unwrap();
    write_message(&mut buf, &msg).unwrap();
    let mut r = std::io::BufReader::new(&buf[..]);
    assert_eq!(read_message(&mut r).unwrap(), Some(msg.clone()));
    assert_eq!(read_message(&mut r).unwrap(), Some(msg));
    assert_eq!(read_message(&mut r).unwrap(), None);
}

#[test]
fn uris() {
    let p = Path::new("/tmp/a b/ü.ppl");
    let u = path_to_uri(p);
    assert_eq!(u, "file:///tmp/a%20b/%C3%BC.ppl");
    assert_eq!(uri_to_path(&u).unwrap(), p);
    assert_eq!(
        uri_to_path("file:///c%3A/x/y.ppl").unwrap(),
        PathBuf::from("c:/x/y.ppl")
    );
}
