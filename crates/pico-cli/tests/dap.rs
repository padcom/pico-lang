//! `pico dap` (spec §14.5, PLAN P10.8c): a scripted Debug Adapter Protocol session, as VS Code
//! runs it: launch a `.pi` program, stop at a breakpoint, read the stack and the variables
//! (with pico's formatters). Skipped where there's no lldb-dap.

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

fn have_lldb_dap() -> bool {
    std::env::var_os("PICO_LLDB_DAP").is_some()
        || [
            "lldb-dap",
            "lldb-dap-23",
            "lldb-dap-22",
            "lldb-dap-21",
            "lldb-dap-20",
        ]
        .iter()
        .any(|n| {
            Command::new(n)
                .arg("--help")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok()
        })
}

fn read(r: &mut impl BufRead) -> Option<Value> {
    let mut len = 0;
    loop {
        let mut line = String::new();
        if r.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some(v) = line.strip_prefix("Content-Length:") {
            len = v.trim().parse().ok()?;
        }
    }
    let mut body = vec![0; len];
    r.read_exact(&mut body).ok()?;
    serde_json::from_slice(&body).ok()
}

struct Session {
    child: Child,
    rx: mpsc::Receiver<Value>,
    seq: u64,
    /// Messages received but not asked for yet.
    seen: Vec<Value>,
}

impl Session {
    fn start(cache: &std::path::Path) -> Session {
        let mut child = Command::new(env!("CARGO_BIN_EXE_pico"))
            .arg("dap")
            .env("PICO_CACHE_DIR", cache)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut out = BufReader::new(child.stdout.take().unwrap());
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            while let Some(m) = read(&mut out) {
                if tx.send(m).is_err() {
                    break;
                }
            }
        });
        Session {
            child,
            rx,
            seq: 0,
            seen: Vec::new(),
        }
    }

    fn send(&mut self, command: &str, arguments: Value) -> u64 {
        self.seq += 1;
        let body = json!({ "seq": self.seq, "type": "request", "command": command, "arguments": arguments }).to_string();
        let stdin = self.child.stdin.as_mut().unwrap();
        write!(stdin, "Content-Length: {}\r\n\r\n{body}", body.len()).unwrap();
        stdin.flush().unwrap();
        self.seq
    }

    /// The first message (seen before or arriving within 60 s) that `pred` accepts.
    fn wait(&mut self, what: &str, pred: impl Fn(&Value) -> bool) -> Value {
        if let Some(i) = self.seen.iter().position(&pred) {
            return self.seen.remove(i);
        }
        loop {
            match self.rx.recv_timeout(Duration::from_secs(60)) {
                Ok(m) if pred(&m) => return m,
                Ok(m) => self.seen.push(m),
                Err(_) => panic!("no {what}; got: {:#?}", self.seen),
            }
        }
    }

    fn request(&mut self, command: &str, arguments: Value) -> Value {
        let seq = self.send(command, arguments);
        self.wait(command, |m| {
            m["type"] == "response" && m["request_seq"] == seq
        })
    }

    fn event(&mut self, name: &str) -> Value {
        self.wait(name, |m| m["type"] == "event" && m["event"] == name)
    }
}

#[test]
fn debugs_a_pico_program() {
    if !have_lldb_dap() {
        eprintln!("note: no lldb-dap: skipping");
        return;
    }
    let dir = std::env::temp_dir().join(format!("pico-dap-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let main = dir.join("main.pi");
    std::fs::write(
        &main,
        "class Person {\n  constructor(public name: string) {}\n}\nfunction show(m: Map<string, i64>, names: string[], greeting: string, who: Person) {\n  println(`${m.size} ${names.length} ${greeting} ${who.name}`)\n}\nconst TITLE = \"report\"\nconst m = new Map<string, i64>()\nm.set(\"a\", 1)\nm.set(\"b\", 2)\nshow(m, [\"x\", \"y\"], \"hello\", new Person(\"Ann\"))\n",
    )
    .unwrap();
    let mut s = Session::start(&dir.join("cache"));
    let r = s.request("initialize", json!({ "adapterID": "pico", "linesStartAt1": true, "columnsStartAt1": true, "pathFormat": "path" }));
    assert_eq!(r["success"], true, "{r:#?}");
    let launch = s.send(
        "launch",
        json!({ "program": main.display().to_string(), "stopOnEntry": false }),
    );
    s.event("initialized");
    let r = s.request(
        "setBreakpoints",
        json!({ "source": { "path": main.display().to_string() }, "breakpoints": [{ "line": 5 }] }),
    );
    assert_eq!(r["body"]["breakpoints"][0]["verified"], true, "{r:#?}");
    s.request("configurationDone", json!({}));
    let r = s.wait("launch", |m| {
        m["type"] == "response" && m["request_seq"] == launch
    });
    assert_eq!(r["success"], true, "{r:#?}");
    let stopped = s.event("stopped");
    let thread = stopped["body"]["threadId"].clone();
    let st = s.request("stackTrace", json!({ "threadId": thread, "levels": 5 }));
    let top = &st["body"]["stackFrames"][0];
    assert!(top["name"].as_str().unwrap().starts_with("show"), "{st:#?}");
    assert_eq!(top["line"], 5, "{st:#?}");
    assert!(
        top["source"]["path"].as_str().unwrap().ends_with("main.pi"),
        "{st:#?}"
    );
    let scopes = s.request("scopes", json!({ "frameId": top["id"] }));
    let locals = scopes["body"]["scopes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|sc| sc["name"].as_str().unwrap_or("").contains("Local") || sc["name"] == "Arguments")
        .unwrap_or_else(|| panic!("{scopes:#?}"))
        .clone();
    let vars = s.request(
        "variables",
        json!({ "variablesReference": locals["variablesReference"] }),
    );
    let list = vars["body"]["variables"].as_array().unwrap().clone();
    let var = |n: &str| {
        list.iter()
            .find(|v| v["name"] == n)
            .unwrap_or_else(|| panic!("{n}: {list:#?}"))
            .clone()
    };
    // Just the text: no address, nothing to expand.
    assert_eq!(var("greeting")["value"], "\"hello\"", "{list:#?}");
    assert_eq!(var("greeting")["variablesReference"], 0, "{list:#?}");
    assert_eq!(var("names")["value"], "length=2", "{list:#?}");
    let m = var("m");
    assert_eq!(m["value"], "size=2", "{list:#?}");
    // The map's entries, by key.
    let entries = s.request(
        "variables",
        json!({ "variablesReference": m["variablesReference"] }),
    );
    let names: Vec<String> = entries["body"]["variables"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| {
            format!(
                "{} = {}",
                v["name"].as_str().unwrap(),
                v["value"].as_str().unwrap()
            )
        })
        .collect();
    assert_eq!(names, ["[\"a\"] = 1", "[\"b\"] = 2"], "{entries:#?}");
    // The top-level bindings, in the globals.
    let globals = scopes["body"]["scopes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|sc| sc["name"].as_str().unwrap_or("").contains("Global"))
        .unwrap_or_else(|| panic!("{scopes:#?}"))
        .clone();
    let g = s.request(
        "variables",
        json!({ "variablesReference": globals["variablesReference"] }),
    );
    let title = g["body"]["variables"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["name"] == "TITLE")
        .unwrap_or_else(|| panic!("{g:#?}"))
        .clone();
    assert_eq!(title["value"], "\"report\"", "{g:#?}");
    // An object's string field: the text, not an address.
    let who = var("who");
    let fields = s.request(
        "variables",
        json!({ "variablesReference": who["variablesReference"] }),
    );
    let name = fields["body"]["variables"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["name"] == "name")
        .unwrap_or_else(|| panic!("{fields:#?}"))
        .clone();
    assert_eq!(name["value"], "\"Ann\"", "{fields:#?}");
    // Watch expressions: the same values as the variables.
    for (expr, want) in [
        ("greeting", "\"hello\""),
        ("who.name", "\"Ann\""),
        ("names", "length=2"),
    ] {
        let r = s.request(
            "evaluate",
            json!({ "expression": expr, "frameId": top["id"], "context": "watch" }),
        );
        assert_eq!(r["body"]["result"], want, "{expr}: {r:#?}");
    }
    s.request("disconnect", json!({ "terminateDebuggee": true }));
    let _ = s.child.wait();

    // A program that doesn't compile fails the launch with the compiler's message.
    let bad = dir.join("bad.pi");
    std::fs::write(&bad, "const x: i32 = \"no\"\n").unwrap();
    let mut s = Session::start(&dir.join("cache"));
    s.request("initialize", json!({ "adapterID": "pico" }));
    let r = s.request("launch", json!({ "program": bad.display().to_string() }));
    assert_eq!(r["success"], false, "{r:#?}");
    assert!(
        r["message"].as_str().unwrap().contains("mismatched types"),
        "{r:#?}"
    );
    let _ = s.child.kill();
}

/// `attach` with `waitFor`: the program starts after the request (another program runs it, with
/// its stdio), the breakpoint is hit, and the formatters are loaded (a string shows as text).
#[test]
fn attaches_to_a_program_when_it_starts() {
    if !have_lldb_dap() {
        eprintln!("note: no lldb-dap: skipping");
        return;
    }
    let dir = std::env::temp_dir().join(format!("pico-dap-attach-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let main = dir.join("echoer.pi");
    std::fs::write(
        &main,
        "import { stdin } from \"std/process\"\nfunction show(text: string) {\n  println(text)\n}\nfor await (const chunk of stdin) {\n  show(new TextDecoder().decode(chunk).trim())\n}\n",
    )
    .unwrap();
    let exe = dir.join("echoer-attach");
    let built = Command::new(env!("CARGO_BIN_EXE_pico"))
        .args(["build", main.to_str().unwrap(), "-o", exe.to_str().unwrap()])
        .env("PICO_CACHE_DIR", dir.join("cache"))
        .status()
        .unwrap();
    assert!(built.success());
    let mut s = Session::start(&dir.join("cache"));
    let r = s.request("initialize", json!({ "adapterID": "pico", "linesStartAt1": true, "columnsStartAt1": true, "pathFormat": "path" }));
    assert_eq!(r["success"], true, "{r:#?}");
    let attach = s.send(
        "attach",
        json!({ "program": exe.display().to_string(), "waitFor": true }),
    );
    // Started by "another program" once the debugger waits, stdin a pipe the test owns.
    std::thread::sleep(Duration::from_secs(1));
    let mut program = Command::new(&exe)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    s.event("initialized");
    let r = s.request(
        "setBreakpoints",
        json!({ "source": { "path": main.display().to_string() }, "breakpoints": [{ "line": 3 }] }),
    );
    // (Not necessarily verified yet: lldb may not have loaded the program's module when this
    // arrives, as on Linux arm64. The stop below shows that it resolves.)
    assert_eq!(r["success"], true, "{r:#?}");
    s.request("configurationDone", json!({}));
    let r = s.wait("attach", |m| {
        m["type"] == "response" && m["request_seq"] == attach
    });
    assert_eq!(r["success"], true, "{r:#?}");
    let stdin = program.stdin.as_mut().unwrap();
    stdin.write_all(b"hello\n").unwrap();
    stdin.flush().unwrap();
    let stopped = s.event("stopped");
    let st = s.request(
        "stackTrace",
        json!({ "threadId": stopped["body"]["threadId"], "levels": 3 }),
    );
    let top = &st["body"]["stackFrames"][0];
    assert!(top["name"].as_str().unwrap().starts_with("show"), "{st:#?}");
    assert_eq!(top["line"], 3, "{st:#?}");
    let r = s.request(
        "evaluate",
        json!({ "expression": "text", "frameId": top["id"], "context": "watch" }),
    );
    assert_eq!(r["body"]["result"], "\"hello\"", "{r:#?}");
    // Detached, the program goes on: it prints the line and ends with its input.
    s.request("disconnect", json!({ "terminateDebuggee": false }));
    let _ = s.child.wait();
    drop(program.stdin.take());
    let out = program.wait_with_output().unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "hello\n");
}
