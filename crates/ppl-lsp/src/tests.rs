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
        json!({ "jsonrpc": "2.0", "id": 2, "method": "textDocument/completion", "params": {} }),
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
