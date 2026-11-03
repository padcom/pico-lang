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
        json!({ "jsonrpc": "2.0", "id": 2, "method": "textDocument/references", "params": {} }),
        json!({ "jsonrpc": "2.0", "id": 3, "method": "shutdown" }),
        json!({ "jsonrpc": "2.0", "method": "exit" }),
    ]);
    assert_eq!(out.len(), 3, "{out:#?}");
    assert_eq!(out[0]["id"], 1);
    let caps = &out[0]["result"]["capabilities"];
    assert_eq!(caps["textDocumentSync"]["change"], 1);
    assert_eq!(caps["documentFormattingProvider"], true);
    assert_eq!(caps["hoverProvider"], true);
    assert_eq!(caps["definitionProvider"], true);
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

/// A hover or definition request at the `n`th (0-based) occurrence of `needle` in `text`
/// (plus `skip` characters into it; ASCII lines).
fn at(id: u64, method: &str, uri: &str, text: &str, needle: &str, n: usize, skip: usize) -> Value {
    let offset = text
        .match_indices(needle)
        .nth(n)
        .unwrap_or_else(|| panic!("no {needle} #{n}"))
        .0
        + skip;
    let line = text[..offset].matches('\n').count();
    let character = offset - text[..offset].rfind('\n').map_or(0, |i| i + 1);
    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": {
        "textDocument": { "uri": uri }, "position": { "line": line, "character": character } } })
}

fn response(out: &[Value], id: u64) -> &Value {
    let m = out
        .iter()
        .find(|m| m["id"] == id && m.get("method").is_none())
        .unwrap_or_else(|| panic!("no response {id}: {out:#?}"));
    &m["result"]
}

fn hover_text(out: &[Value], id: u64) -> String {
    let r = response(out, id);
    assert_eq!(r["contents"]["kind"], "markdown", "{r:#?}");
    r["contents"]["value"].as_str().unwrap().to_string()
}

fn def_line(out: &[Value], id: u64) -> (String, u64, u64) {
    let r = response(out, id);
    (
        r["uri"]
            .as_str()
            .unwrap_or_else(|| panic!("{r:#?}"))
            .to_string(),
        r["range"]["start"]["line"].as_u64().unwrap(),
        r["range"]["start"]["character"].as_u64().unwrap(),
    )
}

const SAMPLE: &str = r#"/** A point on the grid. */
struct Point {
  x: i32
  /// The vertical position.
  y: i32
}

class Counter {
  count = 0
  public bump(by: i64): i64 {
    count += by
    count
  }
}

type Meters = f64

/**
 * Adds two numbers.
 */
function add(a: i32, b: i32): i32 { a + b }

const p: Point = Point { x: 1, y: 2 }
let total = add(p.x, p.y)
const c = new Counter()
c.bump(2)
const m: Meters = 2.0
println(`${total} ${m}`)
"#;

#[test]
fn hover() {
    let dir = tmp("hover");
    let uri = path_to_uri(&dir.join("main.ppl"));
    let h = |id, needle, n, skip| at(id, "textDocument/hover", &uri, SAMPLE, needle, n, skip);
    let out = session(vec![
        init(),
        open(&uri, SAMPLE),
        h(2, "Point", 1, 0),   // the annotation
        h(3, "add", 1, 0),     // the call
        h(4, "p.y", 0, 0),     // `p`
        h(5, "p.y", 0, 2),     // the field
        h(6, "bump", 1, 0),    // the method call
        h(7, "by", 1, 1),      // the parameter's use (inside the name)
        h(8, "Counter", 1, 0), // `new Counter`
        h(9, "Meters", 1, 0),
        h(10, "total", 0, 0),
        h(11, "count", 1, 0), // an implicit `this.count`
        h(12, "\n\n", 0, 1),  // nothing
    ]);
    let t = |id| hover_text(&out, id);
    assert_eq!(t(2), "```ppl\nstruct Point\n```\n\nA point on the grid.");
    assert_eq!(
        t(3),
        "```ppl\nfunction add(a: i32, b: i32): i32\n```\n\nAdds two numbers."
    );
    assert_eq!(t(4), "```ppl\nconst p: Point\n```");
    assert_eq!(
        t(5),
        "```ppl\n(field) Point.y: i32\n```\n\nThe vertical position."
    );
    assert_eq!(t(6), "```ppl\n(method) Counter.bump(by: i64): i64\n```");
    assert_eq!(t(7), "```ppl\n(parameter) by: i64\n```");
    assert_eq!(t(8), "```ppl\nclass Counter\n```");
    assert_eq!(t(9), "```ppl\ntype Meters = f64\n```");
    assert_eq!(t(10), "```ppl\nlet total: i32\n```");
    assert_eq!(t(11), "```ppl\n(field) Counter.count: i64\n```");
    assert!(response(&out, 12).is_null());
    // The range is the name's.
    let r = &response(&out, 3)["range"];
    assert_eq!(r["start"], json!({ "line": 23, "character": 12 }));
    assert_eq!(r["end"], json!({ "line": 23, "character": 15 }));
}

#[test]
fn definition() {
    let dir = tmp("def");
    let uri = path_to_uri(&dir.join("main.ppl"));
    let d = |id, needle, n, skip| at(id, "textDocument/definition", &uri, SAMPLE, needle, n, skip);
    let out = session(vec![
        init(),
        open(&uri, SAMPLE),
        d(2, "Point", 2, 0), // the literal's type
        d(3, "add", 1, 0),
        d(4, "p.x", 0, 2), // the field
        d(5, "bump", 1, 0),
        d(6, "Counter", 1, 0),
        d(7, "a + b", 0, 0),
        d(8, "Meters", 1, 0),
        d(9, "Point { x", 0, 8), // a literal's field
    ]);
    let l = |id| {
        let (u, line, ch) = def_line(&out, id);
        assert_eq!(u, uri);
        (line, ch)
    };
    assert_eq!(l(2), (1, 7));
    assert_eq!(l(3), (20, 9));
    assert_eq!(l(4), (2, 2));
    assert_eq!(l(5), (9, 9));
    assert_eq!(l(6), (7, 6));
    assert_eq!(l(7), (20, 13));
    assert_eq!(l(8), (15, 5));
    assert_eq!(l(9), (2, 2));
}

#[test]
fn definition_across_modules_and_into_the_prelude() {
    let dir = tmp("modules");
    let geo = "export struct Vec2 {\n  x: i32\n  y: i32\n}\n/// The dot product.\nexport function dot(a: Vec2, b: Vec2): i32 { a.x * b.x + a.y * b.y }\n";
    std::fs::write(dir.join("geo.ppl"), geo).unwrap();
    let geo_uri = path_to_uri(&dir.join("geo.ppl").canonicalize().unwrap());
    let main = "import { Vec2, dot } from \"./geo\"\nconst v = Vec2 { x: 1, y: 2 }\nconst e = new Error(\"x\")\nprintln(`${dot(v, v)} ${e.message} ${[1].map((n) => n).length}`)\n";
    let uri = path_to_uri(&dir.join("main.ppl"));
    let d = |id, needle, n, skip| at(id, "textDocument/definition", &uri, main, needle, n, skip);
    let out = session(vec![
        init(),
        open(&uri, main),
        d(2, "dot", 1, 0),  // a call of an imported function
        d(3, "dot", 0, 0),  // the import
        d(4, "Vec2", 1, 0), // an imported struct
        d(6, "Error", 0, 0),
        d(7, "message", 0, 0),
        d(8, "map", 0, 0),
        at(9, "textDocument/hover", &uri, main, "dot", 1, 0),
    ]);
    let canon = |u: String| path_to_uri(&uri_to_path(&u).unwrap().canonicalize().unwrap());
    let (u, line, ch) = def_line(&out, 2);
    assert_eq!((canon(u), line, ch), (geo_uri.clone(), 5, 16));
    let (u, line, _) = def_line(&out, 3);
    assert_eq!((canon(u), line), (geo_uri.clone(), 5));
    let (u, line, ch) = def_line(&out, 4);
    assert_eq!((canon(u), line, ch), (geo_uri.clone(), 0, 14));
    // The core library: the bundled stdlib's file, or a copy of it.
    for (id, file, decl) in [
        (6, "error.ppl", "class Error"),
        (7, "error.ppl", "message"),
        (8, "array.ppl", "__Array_map"),
    ] {
        let (u, line, _) = def_line(&out, id);
        assert!(u.ends_with(&format!("/{file}")), "{u}");
        let text = std::fs::read_to_string(uri_to_path(&u).unwrap()).unwrap();
        let l = text.lines().nth(line as usize).unwrap();
        assert!(l.contains(decl), "{l}");
    }
    assert_eq!(
        hover_text(&out, 9),
        "```ppl\nfunction dot(a: Vec2, b: Vec2): i32\n```\n\nThe dot product."
    );
}

#[test]
fn a_changed_document_with_errors_is_indexed_again() {
    let dir = tmp("stale");
    let uri = path_to_uri(&dir.join("main.ppl"));
    let good = "const answer = 42\nprintln(answer)\n";
    let bad = "const answer = 42\nprintln(answer)\nconst broken: i32 = \nconst more = answer + 1\n";
    let out = session(vec![
        init(),
        open(&uri, good),
        // (A request checks the document first: this builds the index of `good`.)
        at(10, "textDocument/hover", &uri, good, "answer", 1, 0),
        json!({ "jsonrpc": "2.0", "method": "textDocument/didChange", "params": {
            "textDocument": { "uri": uri, "version": 2 },
            "contentChanges": [{ "text": bad }] } }),
        at(2, "textDocument/hover", &uri, bad, "answer", 1, 0),
        at(3, "textDocument/definition", &uri, bad, "answer", 1, 0),
        // (Only in the new text: the recovering pass indexed it.)
        at(4, "textDocument/hover", &uri, bad, "answer", 2, 0),
    ]);
    // The hover was answered after the change was checked (its diagnostics came first).
    let pos = |pred: &dyn Fn(&Value) -> bool| out.iter().position(pred).unwrap();
    let diag = pos(&|m| {
        m["method"] == "textDocument/publishDiagnostics"
            && !m["params"]["diagnostics"].as_array().unwrap().is_empty()
    });
    assert!(diag < pos(&|m| m["id"] == 2), "{out:#?}");
    assert_eq!(hover_text(&out, 2), "```ppl\nconst answer: i64\n```");
    assert_eq!(def_line(&out, 3), (uri.clone(), 0, 6));
    assert_eq!(hover_text(&out, 4), "```ppl\nconst answer: i64\n```");
}

#[test]
fn hover_positions_count_utf16() {
    let dir = tmp("utf16");
    let uri = path_to_uri(&dir.join("main.ppl"));
    // `𝄞` is two UTF-16 code units, `ż` one; `n` starts at character 15.
    let text = "const n = 1\nprintln(\"𝄞ż\", n)\n";
    let req = |id, character| {
        json!({ "jsonrpc": "2.0", "id": id, "method": "textDocument/hover",
        "params": { "textDocument": { "uri": uri }, "position": { "line": 1, "character": character } } })
    };
    let out = session(vec![init(), open(&uri, text), req(2, 15), req(3, 13)]);
    assert_eq!(hover_text(&out, 2), "```ppl\nconst n: i64\n```");
    let r = &response(&out, 2)["range"];
    assert_eq!(r["start"]["character"], 15);
    assert_eq!(r["end"]["character"], 16);
    assert!(response(&out, 3).is_null());
}

#[test]
fn embedded_files_get_a_read_only_copy() {
    let name = format!("<js:lsp-test-{}>", std::process::id());
    for text in ["export function a() {}\n", "export function b() {}\n"] {
        let mut sm = SourceMap::new();
        let f = sm.add(name.clone(), text);
        let path = uri_to_path(&file_uri(&sm, f).unwrap()).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
        assert!(std::fs::metadata(&path).unwrap().permissions().readonly());
    }
}

/// Opens `text` and asks for a hover at the `n`th `needle` (`skip` characters in) and the
/// definition of the `m`th `def_needle`: the diagnostics, the hover and the definition's line.
fn ask(
    name: &str,
    text: &str,
    hover: (&str, usize, usize),
    def: (&str, usize),
) -> (usize, String, u64) {
    let dir = tmp(name);
    let uri = path_to_uri(&dir.join("main.ppl"));
    let out = session(vec![
        init(),
        open(&uri, text),
        at(
            2,
            "textDocument/hover",
            &uri,
            text,
            hover.0,
            hover.1,
            hover.2,
        ),
        at(3, "textDocument/definition", &uri, text, def.0, def.1, 0),
    ]);
    let d = diags_for(&out, &uri);
    let n = d.last().unwrap()["params"]["diagnostics"]
        .as_array()
        .unwrap()
        .len();
    (n, hover_text(&out, 2), def_line(&out, 3).1)
}

#[test]
fn answers_in_code_with_syntax_errors() {
    // A half-written method: the rest of the class is still there.
    let text = "class Box {\n  constructor(public v: i32) {}\n  public half(\n  public get(): i32 { v }\n}\nconst b = new Box(1)\nprintln(b.get())\n";
    let (n, h, line) = ask("syntax1", text, ("b.get", 0, 2), ("get", 1));
    assert_eq!(n, 1, "only the parser's error is shown");
    assert_eq!(h, "```ppl\n(method) Box.get(): i32\n```");
    assert_eq!(line, 3);
    // A half-written expression, and member access right after it.
    let text = "struct Point {\n  x: i32\n  y: i32\n}\nconst p = Point { x: 1, y: 2 }\nfunction f(n: i32): i32 {\n  const k = n *\n  const j = k + p.y\n  j\n}\n";
    let (n, h, line) = ask("syntax2", text, ("p.y", 0, 2), ("k + ", 0));
    assert_eq!(n, 1);
    assert_eq!(h, "```ppl\n(field) Point.y: i32\n```");
    assert_eq!(line, 6);
    // A field without a type.
    let text = "struct P {\n  x: i32\n  y:\n}\nconst p = P { x: 1 }\nprintln(p.x)\n";
    let (_, h, _) = ask("syntax3", text, ("y:", 0, 0), ("p.x", 0));
    assert_eq!(h, "```ppl\n(field) P.y: unknown\n```");
}

#[test]
fn answers_in_code_with_name_and_type_errors() {
    let text = "function add(a: i32, b: i32): i32 { a + b }\nconst r = add(1, missing)\nconst s = add(r, 2)\nprintln(s)\n";
    let (n, h, line) = ask("names", text, ("s)", 0, 0), ("add", 2));
    assert_eq!(n, 1, "no follow-up errors");
    assert_eq!(h, "```ppl\nconst s: i32\n```");
    assert_eq!(line, 0);
    let text = "function g(x: Missing, y: i32): i32 { y }\nconst k = g(1, 2)\n";
    let (n, h, _) = ask("types", text, ("g(1", 0, 0), ("g(1", 0));
    assert_eq!(n, 1);
    assert_eq!(h, "```ppl\nfunction g(x: unknown, y: i32): i32\n```");
}

#[test]
fn constructors_this_is_not_a_name() {
    let text = "class Box {\n  constructor(public v: i32) {}\n}\nprintln(new Box(1).v)\n";
    let dir = tmp("this");
    let uri = path_to_uri(&dir.join("main.ppl"));
    let out = session(vec![
        init(),
        open(&uri, text),
        at(2, "textDocument/hover", &uri, text, "constructor", 0, 3),
    ]);
    assert!(response(&out, 2).is_null(), "{out:#?}");
}

/// The recovering front end on fixtures with pieces cut out: no panics, always types.
#[test]
fn recovery_never_panics() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/run");
    for f in [
        "classes.ppl",
        "generics.ppl",
        "enums.ppl",
        "closures.ppl",
        "destructuring.ppl",
    ] {
        let path = root.join(f);
        let text = std::fs::read_to_string(&path).unwrap();
        let dir = path.parent().unwrap().to_path_buf();
        let mut cut = 0;
        while cut < text.len() {
            let end = (cut + 7).min(text.len());
            if text.is_char_boundary(cut) && text.is_char_boundary(end) {
                let t = format!("{}{}", &text[..cut], &text[end..]);
                let c = ppl_driver::check_source_recovering(
                    path.display().to_string(),
                    t,
                    &dir,
                    ppl_driver::BuildOptions::default(),
                );
                assert!(c.types.is_some(), "{f} without {cut}..{end}");
            }
            cut += 151;
        }
    }
}

/// Completion at `‸` in `text` (the marker is removed): the items' labels, and the items.
fn complete_at(name: &str, text: &str, files: &[(&str, &str)]) -> (Vec<String>, Vec<Value>) {
    let dir = tmp(name);
    for (f, t) in files {
        std::fs::write(dir.join(f), t).unwrap();
    }
    let at = text.find('‸').expect("a cursor");
    let text = text.replace('‸', "");
    let line = text[..at].matches('\n').count();
    let character: usize = text[text[..at].rfind('\n').map_or(0, |i| i + 1)..at]
        .chars()
        .map(char::len_utf16)
        .sum();
    let uri = path_to_uri(&dir.join("main.ppl"));
    let out = session(vec![
        init(),
        open(&uri, &text),
        json!({ "jsonrpc": "2.0", "id": 2, "method": "textDocument/completion", "params": {
            "textDocument": { "uri": uri }, "position": { "line": line, "character": character } } }),
    ]);
    let r = response(&out, 2);
    assert_eq!(r["isIncomplete"], false);
    let items = r["items"].as_array().unwrap().clone();
    let labels = items
        .iter()
        .map(|i| i["label"].as_str().unwrap().to_string())
        .collect();
    (labels, items)
}

fn item<'a>(items: &'a [Value], label: &str) -> &'a Value {
    items
        .iter()
        .find(|i| i["label"] == label)
        .unwrap_or_else(|| panic!("no {label}: {items:#?}"))
}

const CLASSES: &str = "struct Point {\n  x: i32\n  /// The vertical position.\n  y: i32\n}\n\nclass Counter {\n  private secret = 1\n  public static zero(): Counter { new Counter() }\n  public get double(): i64 { secret * 2 }\n  public bump(by: i64): i64 {\n    secret += by\n    secret\n  }\n}\n\nenum Color { Red, Green }\n\nconst p = Point { x: 1, y: 2 }\nconst c = new Counter()\nconst xs = [1, 2]\nconst s = \"hi\"\n";

#[test]
fn completes_members() {
    let (l, items) = complete_at("members1", &format!("{CLASSES}p.‸\nprintln(1)\n"), &[]);
    assert_eq!(l, ["x", "y"]);
    let y = item(&items, "y");
    assert_eq!(y["kind"], 5);
    assert_eq!(y["detail"], "(field) Point.y: i32");
    assert_eq!(y["documentation"]["value"], "The vertical position.");

    // A partly typed name: the client filters, the server lists them all.
    let (l, items) = complete_at("members2", &format!("{CLASSES}println(c.bu‸)\n"), &[]);
    assert_eq!(
        l,
        ["double", "bump"],
        "private members aren't offered outside the class"
    );
    assert_eq!(item(&items, "bump")["kind"], 2);
    assert_eq!(
        item(&items, "bump")["detail"],
        "(method) Counter.bump(by: i64): i64"
    );
    assert_eq!(item(&items, "double")["kind"], 10);

    let inside = CLASSES.replace("    secret += by\n", "    this.‸\n    secret += by\n");
    let (l, _) = complete_at("members3", &inside, &[]);
    assert!(l.contains(&"secret".to_string()), "{l:?}");

    let (l, items) = complete_at("members4", &format!("{CLASSES}xs.‸\n"), &[]);
    for m in ["length", "push", "pop", "map", "filter"] {
        assert!(l.contains(&m.to_string()), "{m}: {l:?}");
    }
    assert!(
        item(&items, "map")["detail"]
            .as_str()
            .unwrap()
            .starts_with("(method) Array.map")
    );
    let (l, _) = complete_at("members5", &format!("{CLASSES}s.‸\n"), &[]);
    for m in ["length", "slice", "includes", "toString"] {
        assert!(l.contains(&m.to_string()), "{m}: {l:?}");
    }
}

#[test]
fn completes_statics_and_variants() {
    let (l, _) = complete_at("statics", &format!("{CLASSES}Counter.‸\n"), &[]);
    assert_eq!(l, ["zero"]);
    let (l, items) = complete_at("variants", &format!("{CLASSES}const k = Color.‸\n"), &[]);
    assert_eq!(l, ["Red", "Green"]);
    assert_eq!(item(&items, "Red")["kind"], 20);
}

#[test]
fn completes_names_in_scope() {
    let geo = "export struct Vec2 {\n  x: i32\n  y: i32\n}\n/// The dot product.\nexport function dot(a: Vec2, b: Vec2): i32 { a.x * b.x + a.y * b.y }\n";
    let text = "import { dot } from \"./geo\"\nimport * as geo from \"./geo\"\n\n/** Doubles. */\nfunction twice(n: i32): i32 { n * 2 }\n\nfunction f(n: i32): i32 {\n  const before = n\n  if (n > 0) {\n    const inner = 1\n  }\n  ‸\n  const after = 2\n  before\n}\nconst top = 1\n";
    let (l, items) = complete_at("scope", text, &[("geo.ppl", geo)]);
    let has = |n: &str| l.iter().any(|x| x == n);
    for n in [
        "n", "before", "twice", "f", "top", "dot", "geo", "Error", "println", "i32", "const",
        "return",
    ] {
        assert!(has(n), "{n}: {l:?}");
    }
    for n in ["inner", "after", "__ppl_cursor"] {
        assert!(!has(n), "{n} isn't in scope: {l:?}");
    }
    assert!(!l.iter().any(|x| x.starts_with("__")), "{l:?}");
    // Nearest first: the locals come before the file's declarations.
    let pos = |n: &str| l.iter().position(|x| x == n).unwrap();
    assert!(pos("before") < pos("twice") && pos("twice") < pos("Error"));
    assert_eq!(item(&items, "before")["kind"], 21);
    assert_eq!(item(&items, "n")["kind"], 6);
    assert_eq!(
        item(&items, "twice")["detail"],
        "function twice(n: i32): i32"
    );
    assert_eq!(item(&items, "twice")["documentation"]["value"], "Doubles.");
    assert_eq!(
        item(&items, "dot")["documentation"]["value"],
        "The dot product."
    );
    assert_eq!(item(&items, "geo")["kind"], 9);
    assert_eq!(
        item(&items, "println")["detail"],
        "function println(value?): void"
    );
    assert_eq!(item(&items, "Error")["kind"], 7);
}

#[test]
fn completes_imported_names() {
    let geo = "export struct Vec2 {\n  x: i32\n  y: i32\n}\nexport function dot(a: Vec2, b: Vec2): i32 { a.x * b.x }\nfunction helper(): i32 { 1 }\nexport const ORIGIN = 0\n";
    let (l, items) = complete_at(
        "imports",
        "import { d‸ } from \"./geo\"\n",
        &[("geo.ppl", geo)],
    );
    assert_eq!(l, ["Vec2", "dot", "ORIGIN"]);
    assert_eq!(
        item(&items, "dot")["detail"],
        "function dot(a: Vec2, b: Vec2): i32 { a.x * b.x }"
    );
    assert_eq!(item(&items, "Vec2")["kind"], 22);
    let person = "export class Person {\n  constructor(public name: string) {}\n}\n";
    let (l, items) = complete_at(
        "imports_single",
        "import { ‸ } from './person'\n",
        &[("person.ppl", person)],
    );
    assert_eq!(l, ["Person"]);
    assert_eq!(item(&items, "Person")["kind"], 7);
    let (l, _) = complete_at("imports_std", "import { ‸ } from \"std/path\"\n", &[]);
    assert!(l.iter().any(|x| x == "join"), "{l:?}");
}

#[test]
fn completes_members_before_an_existing_name() {
    // An interface-typed value, with the cursor between `.` and a name already there.
    let base = "interface Speakable { speak(s: string): string }\nclass Person implements Speakable {\n  public speak(s: string): string { s }\n}\nconst x: Speakable = new Person()\n";
    for tail in ["x.‸speak('...')\n", "println(x.sp‸eak(\"...\"))\n", "x.‸\n"] {
        let (l, items) = complete_at("before_name", &format!("{base}{tail}"), &[]);
        assert_eq!(l, ["speak"], "{tail}");
        assert_eq!(
            item(&items, "speak")["detail"],
            "(method) Speakable.speak(s: string): string"
        );
    }
}

fn pos_of(text: &str, needle: &str, n: usize, skip: usize) -> Value {
    let offset = text
        .match_indices(needle)
        .nth(n)
        .unwrap_or_else(|| panic!("no {needle}"))
        .0
        + skip;
    let line = text[..offset].matches('\n').count();
    let character = offset - text[..offset].rfind('\n').map_or(0, |i| i + 1);
    json!({ "line": line, "character": character })
}

/// Applies one file's text edits.
fn apply(text: &str, edits: &Value) -> String {
    let mut es: Vec<(usize, usize, String)> = edits
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            let o = |p: &Value| {
                offset_of(
                    text,
                    p["line"].as_u64().unwrap() as usize,
                    p["character"].as_u64().unwrap() as usize,
                )
                .unwrap() as usize
            };
            (
                o(&e["range"]["start"]),
                o(&e["range"]["end"]),
                e["newText"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    es.sort_by_key(|a| std::cmp::Reverse(a.0));
    let mut out = text.to_string();
    for (s, e, t) in es {
        out.replace_range(s..e, &t);
    }
    out
}

/// Renames at the `n`th `needle` (+`skip`) of `main.ppl` (open, the other `files` on disk):
/// the edit by file name, or the error message.
fn rename_at(
    name: &str,
    main: &str,
    files: &[(&str, &str)],
    needle: (&str, usize, usize),
    new_name: &str,
) -> Result<BTreeMap<String, String>, String> {
    let dir = tmp(name);
    for (f, t) in files {
        let p = dir.join(f);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, t).unwrap();
    }
    std::fs::write(dir.join("main.ppl"), main).unwrap();
    let uri = path_to_uri(&dir.join("main.ppl"));
    let pos = pos_of(main, needle.0, needle.1, needle.2);
    let out = session(vec![
        init(),
        open(&uri, main),
        json!({ "jsonrpc": "2.0", "id": 2, "method": "textDocument/rename", "params": {
            "textDocument": { "uri": uri }, "position": pos, "newName": new_name } }),
    ]);
    let m = out.iter().find(|m| m["id"] == 2).unwrap();
    if let Some(e) = m.get("error") {
        return Err(e["message"].as_str().unwrap().to_string());
    }
    let canon = dir.canonicalize().unwrap();
    let mut texts = BTreeMap::new();
    for (u, edits) in m["result"]["changes"].as_object().unwrap() {
        let p = uri_to_path(u).unwrap().canonicalize().unwrap();
        let rel = p.strip_prefix(&canon).unwrap().display().to_string();
        let before = if rel == "main.ppl" {
            main.to_string()
        } else {
            std::fs::read_to_string(&p).unwrap()
        };
        texts.insert(rel, apply(&before, edits));
    }
    Ok(texts)
}

#[test]
fn renames_locals_and_parameters() {
    let main = "function f(n: i32): i32 {\n  let total = n\n  total += n\n  total\n}\nconst total = 1\nprintln(f(total))\n";
    let r = rename_at("rename_local", main, &[], ("total", 1, 0), "sum").unwrap();
    assert_eq!(
        r["main.ppl"],
        "function f(n: i32): i32 {\n  let sum = n\n  sum += n\n  sum\n}\nconst total = 1\nprintln(f(total))\n"
    );
    let r = rename_at("rename_param", main, &[], ("n: i32", 0, 0), "k").unwrap();
    assert_eq!(
        r["main.ppl"],
        "function f(k: i32): i32 {\n  let total = k\n  total += k\n  total\n}\nconst total = 1\nprintln(f(total))\n"
    );
}

#[test]
fn renames_across_the_projects_modules() {
    let geo = "export struct Vec2 {\n  x: i32\n  y: i32\n}\nexport function dot(a: Vec2, b: Vec2): i32 { a.x * b.x + a.y * b.y }\n";
    // (`lib/other.ppl` imports `geo` too, but isn't imported by `main.ppl`.)
    let other =
        "import { dot, Vec2 } from \"../geo\"\nconst v = Vec2 { x: 1, y: 1 }\nprintln(dot(v, v))\n";
    let main = "import { dot, Vec2 } from \"./geo\"\nconst v = Vec2 { x: 2, y: 3 }\nprintln(dot(v, v) + v.x)\n";
    let files = [
        ("geo.ppl", geo),
        ("lib/other.ppl", other),
        ("project.json", "{}"),
    ];
    let r = rename_at("rename_modules", main, &files, ("dot(v", 0, 0), "inner").unwrap();
    assert_eq!(r.len(), 3, "{r:#?}");
    assert!(
        r["geo.ppl"].contains("export function inner(a: Vec2"),
        "{}",
        r["geo.ppl"]
    );
    assert_eq!(r["main.ppl"], main.replace("dot", "inner"));
    assert_eq!(r["lib/other.ppl"], other.replace("dot", "inner"));
    // A field: its declaration, literals and member access, in every module.
    let r = rename_at("rename_field", main, &files, ("v.x", 0, 2), "first").unwrap();
    assert_eq!(
        r["geo.ppl"],
        geo.replace("  x: i32", "  first: i32")
            .replace("a.x * b.x", "a.first * b.first")
    );
    assert_eq!(
        r["main.ppl"],
        main.replace("{ x: 2", "{ first: 2")
            .replace("v.x", "v.first")
    );
    assert_eq!(r["lib/other.ppl"], other.replace("{ x: 1", "{ first: 1"));
}

#[test]
fn renames_methods_with_their_family() {
    let main = "interface Speakable { speak(s: string): string }\nclass Person implements Speakable {\n  public speak(s: string): string { s }\n}\nclass Loud extends Person {\n  public override speak(s: string): string { s }\n}\nclass Other {\n  public speak(): string { \"\" }\n}\nconst x: Speakable = new Person()\nprintln(x.speak(\"a\") + new Loud().speak(\"b\") + new Other().speak())\n";
    let r = rename_at("rename_method", main, &[], ("speak", 1, 0), "say").unwrap();
    assert_eq!(
        r["main.ppl"],
        main.replacen("speak", "say", 3)
            .replace("x.speak", "x.say")
            .replace("Loud().speak", "Loud().say"),
        "`Other.speak` isn't related"
    );
}

#[test]
fn renames_shorthand_fields() {
    let main = "struct P {\n  x: i32\n}\nconst x = 1\nconst p = P { x }\nprintln(p.x + x)\n";
    let r = rename_at("rename_short1", main, &[], ("p.x", 0, 2), "px").unwrap();
    assert_eq!(
        r["main.ppl"],
        "struct P {\n  px: i32\n}\nconst x = 1\nconst p = P { px: x }\nprintln(p.px + x)\n"
    );
    let r = rename_at("rename_short2", main, &[], ("+ x", 0, 2), "v").unwrap();
    assert_eq!(
        r["main.ppl"],
        "struct P {\n  x: i32\n}\nconst v = 1\nconst p = P { x: v }\nprintln(p.x + v)\n"
    );
}

#[test]
fn refuses_what_cant_be_renamed() {
    let main = "const e = new Error(\"x\")\nprintln(e.message)\nconst n = 1\n";
    let err = |needle, new| rename_at("rename_refuse", main, &[], needle, new).unwrap_err();
    assert!(err(("Error", 0, 0), "Oops").contains("core library"));
    assert!(err(("message", 0, 0), "msg").contains("core library"));
    assert!(err(("println", 0, 0), "p").contains("nothing to rename"));
    assert!(err(("n = 1", 0, 0), "while").contains("keyword"));
    assert!(err(("n = 1", 0, 0), "1x").contains("isn't a name"));
    // `prepareRename`: the name's range and text.
    let dir = tmp("prepare");
    let uri = path_to_uri(&dir.join("main.ppl"));
    let out = session(vec![
        init(),
        open(&uri, main),
        json!({ "jsonrpc": "2.0", "id": 2, "method": "textDocument/prepareRename", "params": {
            "textDocument": { "uri": uri }, "position": pos_of(main, "e.message", 0, 0) } }),
    ]);
    let r = response(&out, 2);
    assert_eq!(r["placeholder"], "e");
    assert_eq!(r["range"]["start"], json!({ "line": 1, "character": 8 }));
}

#[test]
fn document_symbols() {
    let text = "struct Point {\n  x: i32\n  y: i32\n}\nclass Counter {\n  count = 0\n  constructor() {}\n  public get double(): i64 { count * 2 }\n  public bump() { count++ }\n}\nenum Color { Red, Green }\ntype Meters = f64\nfunction f(): i32 { 1 }\nconst [a, b] = [1, 2]\nlet broken = \n";
    let dir = tmp("symbols");
    let uri = path_to_uri(&dir.join("main.ppl"));
    let out = session(vec![
        init(),
        open(&uri, text),
        json!({ "jsonrpc": "2.0", "id": 2, "method": "textDocument/documentSymbol", "params": {
            "textDocument": { "uri": uri } } }),
    ]);
    let r = response(&out, 2).as_array().unwrap().clone();
    let names: Vec<(&str, u64)> = r
        .iter()
        .map(|s| (s["name"].as_str().unwrap(), s["kind"].as_u64().unwrap()))
        .collect();
    assert_eq!(
        names,
        [
            ("Point", 23),
            ("Counter", 5),
            ("Color", 10),
            ("Meters", 26),
            ("f", 12),
            ("a", 14),
            ("b", 14),
            ("broken", 13)
        ]
    );
    let kids = |i: usize| -> Vec<(String, u64)> {
        r[i]["children"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| {
                (
                    s["name"].as_str().unwrap().to_string(),
                    s["kind"].as_u64().unwrap(),
                )
            })
            .collect()
    };
    assert_eq!(kids(0), [("x".into(), 8), ("y".into(), 8)]);
    assert_eq!(
        kids(1),
        [
            ("count".into(), 8),
            ("constructor".into(), 9),
            ("double".into(), 7),
            ("bump".into(), 6)
        ]
    );
    assert_eq!(kids(2), [("Red".into(), 22), ("Green".into(), 22)]);
    // Ranges: the declaration; the selection: its name.
    assert_eq!(
        r[4]["selectionRange"]["start"],
        json!({ "line": 12, "character": 9 })
    );
    assert_eq!(
        r[4]["range"]["start"],
        json!({ "line": 12, "character": 0 })
    );
}
