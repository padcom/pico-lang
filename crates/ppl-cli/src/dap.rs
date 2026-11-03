//! `ppl dap` (spec §14.5, PLAN P10.8c): the debug adapter VS Code talks to. It runs lldb-dap
//! and passes the Debug Adapter Protocol through, with two changes to `launch`: a `program`
//! that is a `.ppl` file is built first (a debug build in the cache directory, skipped when
//! nothing changed) and replaced by the executable, and lldb loads ppl's formatters
//! (`ppl_lldb.py`: strings, arrays, maps, sets). A build that fails fails the `launch`, with
//! the compiler's messages.

use serde_json::{Value, json};
use std::io::{BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::{Arc, Mutex};

/// The formatters (`command script import`ed into lldb).
const FORMATTERS: &str = include_str!("ppl_lldb.py");

pub fn run() -> ExitCode {
    let Some(adapter) = lldb_dap() else {
        eprintln!(
            "error: lldb-dap not found (install LLVM's lldb, or set PPL_LLDB_DAP to its path)"
        );
        return ExitCode::FAILURE;
    };
    let formatters = match write_formatters() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: cannot write the lldb formatters: {e}");
            return ExitCode::FAILURE;
        }
    };
    let mut child = match Command::new(&adapter)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: cannot run {}: {e}", adapter.display());
            return ExitCode::FAILURE;
        }
    };
    let out = Arc::new(Mutex::new(std::io::stdout()));
    // lldb-dap → the client, message by message (our own responses interleave).
    let mut from = BufReader::new(child.stdout.take().expect("piped"));
    let to_client = out.clone();
    let pump = std::thread::spawn(move || {
        while let Ok(Some(msg)) = ppl_lsp::read_message(&mut from) {
            let mut o = to_client.lock().expect("stdout");
            if ppl_lsp::write_message(&mut *o, &msg).is_err() {
                break;
            }
        }
    });
    let mut to_adapter = child.stdin.take().expect("piped");
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let mut seq = 1_000_000;
    while let Ok(Some(mut msg)) = ppl_lsp::read_message(&mut input) {
        if msg["type"] == "request"
            && msg["command"] == "launch"
            && let Err(message) = prepare_launch(&mut msg["arguments"], &formatters)
        {
            seq += 1;
            let reply = json!({
                "seq": seq,
                "type": "response",
                "request_seq": msg["seq"],
                "success": false,
                "command": "launch",
                "message": message,
                "body": { "error": { "id": 1, "format": message, "showUser": true } },
            });
            let mut o = out.lock().expect("stdout");
            let _ = ppl_lsp::write_message(&mut *o, &reply);
            continue;
        }
        if ppl_lsp::write_message(&mut to_adapter, &msg).is_err() {
            break;
        }
    }
    drop(to_adapter);
    let status = child.wait();
    let _ = pump.join();
    match status {
        Ok(s) if s.success() => ExitCode::SUCCESS,
        _ => ExitCode::FAILURE,
    }
}

/// lldb-dap: `PPL_LLDB_DAP`, or on PATH (also versioned, as apt.llvm.org installs it), or
/// Xcode's on macOS.
fn lldb_dap() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("PPL_LLDB_DAP").filter(|p| !p.is_empty()) {
        return Some(PathBuf::from(p));
    }
    let works = |name: &str| {
        Command::new(name)
            .arg("--help")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok()
    };
    let names = ["lldb-dap".to_string()]
        .into_iter()
        .chain((18..=30).rev().map(|v| format!("lldb-dap-{v}")))
        .chain(["lldb-vscode".to_string()]);
    for n in names {
        if works(&n) {
            return Some(PathBuf::from(n));
        }
    }
    if cfg!(target_os = "macos") {
        let out = Command::new("xcrun")
            .args(["-f", "lldb-dap"])
            .output()
            .ok()?;
        let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
        return (out.status.success() && !p.is_empty()).then(|| PathBuf::from(p));
    }
    None
}

/// Writes `ppl_lldb.py` to the cache directory (the module's name is the file's).
fn write_formatters() -> std::io::Result<PathBuf> {
    let dir = ppl_driver::fingerprint::cache_dir()
        .join("ppl")
        .join("lldb")
        .join(env!("CARGO_PKG_VERSION"));
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("ppl_lldb.py");
    if std::fs::read_to_string(&path).ok().as_deref() != Some(FORMATTERS) {
        std::fs::write(&path, FORMATTERS)?;
    }
    Ok(path)
}

/// Builds a `.ppl` `program` and points the launch at the executable; adds the formatters.
fn prepare_launch(args: &mut Value, formatters: &Path) -> Result<(), String> {
    let cwd = args["cwd"].as_str().map(PathBuf::from);
    if let Some(program) = args["program"].as_str()
        && program.ends_with(".ppl")
    {
        let mut path = PathBuf::from(program);
        if path.is_relative()
            && let Some(c) = &cwd
        {
            path = c.join(path);
        }
        let exe = build(&path)?;
        args["program"] = json!(exe.display().to_string());
        if cwd.is_none()
            && let Some(dir) = path.parent()
        {
            args["cwd"] = json!(dir.display().to_string());
        }
    }
    let mut init = vec![json!(format!(
        "command script import \"{}\"",
        formatters.display()
    ))];
    if let Some(more) = args["initCommands"].as_array() {
        init.extend(more.iter().cloned());
    }
    args["initCommands"] = json!(init);
    Ok(())
}

/// A debug build of `entry` in the cache directory (kept while nothing changes).
fn build(entry: &Path) -> Result<PathBuf, String> {
    let text = std::fs::read_to_string(entry)
        .map_err(|e| format!("cannot read {}: {e}", entry.display()))?;
    let canon = entry.canonicalize().unwrap_or_else(|_| entry.to_path_buf());
    let key = {
        use std::hash::{DefaultHasher, Hasher};
        let mut h = DefaultHasher::new();
        h.write(canon.display().to_string().as_bytes());
        format!("{:016x}", h.finish())
    };
    let stem = entry
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    let dir = ppl_driver::fingerprint::cache_dir()
        .join("ppl")
        .join("debug")
        .join(key);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let exe = dir.join(if cfg!(windows) {
        format!("{stem}.exe")
    } else {
        stem
    });
    let opts = ppl_driver::BuildOptions::default();
    if ppl_driver::fingerprint::up_to_date(entry, &exe, &opts) {
        return Ok(exe);
    }
    let dir_of = entry.parent().unwrap_or(Path::new("."));
    let checked = ppl_driver::check_source_with(entry.display().to_string(), text, dir_of, opts);
    if checked.diags.iter().any(|d| d.is_error()) {
        return Err(format!(
            "{} doesn't compile:\n{}",
            entry.display(),
            strip_ansi(&checked.render_diagnostics())
        ));
    }
    ppl_driver::build_native(&checked, &exe, opts)?;
    ppl_driver::fingerprint::record(&checked, entry, &exe, &opts);
    let _ = std::io::stderr().flush();
    Ok(exe)
}

/// The rendered diagnostics without terminal colors (for the editor's message).
fn strip_ansi(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            for d in chars.by_ref() {
                if d.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}
