//! The VS Code extension (`editors/vscode`, PLAN P10.6a): its manifest names files that exist,
//! the TextMate grammar knows every keyword the lexer does, and its regular expressions
//! compile (checked with Node.js's `RegExp`, close enough to Oniguruma for the features the
//! grammar uses; skipped where `node` isn't installed).

use ppl_syntax::token::Kw;
use serde_json::Value;
use std::path::{Path, PathBuf};

fn ext_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../editors/vscode")
}

fn json(path: &Path) -> Value {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// Every regular expression in a grammar (`match`, `begin`, `end`, `while`), with its path.
fn regexes(v: &Value, at: &str, out: &mut Vec<(String, String)>) {
    match v {
        Value::Object(m) => {
            for (k, x) in m {
                let here = format!("{at}/{k}");
                match (k.as_str(), x) {
                    ("match" | "begin" | "end" | "while", Value::String(s)) => {
                        out.push((here, s.clone()))
                    }
                    _ => regexes(x, &here, out),
                }
            }
        }
        Value::Array(a) => {
            for (i, x) in a.iter().enumerate() {
                regexes(x, &format!("{at}/{i}"), out);
            }
        }
        _ => {}
    }
}

#[test]
fn manifest() {
    let dir = ext_dir();
    let pkg = json(&dir.join("package.json"));
    let langs = pkg["contributes"]["languages"].as_array().unwrap();
    assert_eq!(langs[0]["id"], "ppl");
    assert_eq!(langs[0]["extensions"][0], ".ppl");
    let conf = dir.join(langs[0]["configuration"].as_str().unwrap());
    json(&conf);
    for g in pkg["contributes"]["grammars"].as_array().unwrap() {
        let grammar = json(&dir.join(g["path"].as_str().unwrap()));
        assert_eq!(grammar["scopeName"], g["scopeName"]);
    }
}

#[test]
fn keywords() {
    let g = json(&ext_dir().join("syntaxes/ppl.tmLanguage.json"));
    let mut all = Vec::new();
    regexes(&g["repository"], "", &mut all);
    let words: Vec<&str> = all
        .iter()
        .flat_map(|(_, r)| r.split(|c: char| !c.is_ascii_alphanumeric() && c != '_'))
        .collect();
    let missing: Vec<&str> = Kw::ALL
        .iter()
        .map(|k| k.as_str())
        .filter(|k| !words.contains(k))
        .collect();
    assert!(
        missing.is_empty(),
        "keywords missing from the grammar: {missing:?}"
    );
}

#[test]
fn regexes_compile() {
    if std::process::Command::new("node")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("note: no Node.js: skipping the grammar's regular expressions");
        return;
    }
    let dir = ext_dir().join("syntaxes");
    let mut all = Vec::new();
    for f in ["ppl.tmLanguage.json", "ppl.markdown.tmLanguage.json"] {
        regexes(&json(&dir.join(f)), f, &mut all);
    }
    // (`\G`, Oniguruma's "where the last match ended", and `(?i:…)` groups aren't JavaScript;
    // they're checked as `^` and plain groups.)
    let list: Vec<Value> = all
        .iter()
        .map(|(at, r)| {
            let js = r.replace("\\G", "^").replace("(?i:", "(?:");
            serde_json::json!([at, js])
        })
        .collect();
    let script = "const list = JSON.parse(require('fs').readFileSync(0, 'utf8'));\n\
        let bad = 0;\n\
        for (const [at, r] of list) { try { new RegExp(r); } catch (e) { bad++; console.log(at + ': ' + e.message); } }\n\
        process.exit(bad ? 1 : 0);";
    let mut child = std::process::Command::new("node")
        .args(["-e", script])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(Value::Array(list).to_string().as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "invalid regular expressions:\n{}",
        String::from_utf8_lossy(&out.stdout)
    );
}
