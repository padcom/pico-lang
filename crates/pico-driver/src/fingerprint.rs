//! Build fingerprints (spec §14.6, PLAN P10.5a): what an output was built from, so that a
//! build with nothing changed can do nothing.
//!
//! A fingerprint records the compiler's identity, the options, target and relevant environment,
//! and a hash of every input: the source files the check read (the entry, imported modules,
//! standard library modules), the files `comptime.readFile` read, and each `project.json` from
//! the entry's directory up (or that there was none). It also records the output's size and
//! modification time, so a replaced or deleted output builds again. Fingerprints live in the
//! user's cache directory, keyed by the output's path. Programs that import C headers or
//! JavaScript modules (inputs outside the compiler's view: system headers, pkg-config) and
//! browser builds (several outputs) aren't fingerprinted: they always build.

use crate::{BuildOptions, Checked, Target, target};
use serde_json::{Value, json};
use std::hash::{DefaultHasher, Hasher};
use std::path::{Path, PathBuf};

/// Whether `exe` is up to date: built from these inputs with these options, and unchanged.
pub fn up_to_date(entry: &Path, exe: &Path, opts: &BuildOptions) -> bool {
    if !tracked(opts) {
        return false;
    }
    let Ok(text) = std::fs::read_to_string(record_path(exe)) else {
        return false;
    };
    let Ok(rec) = serde_json::from_str::<Value>(&text) else {
        return false;
    };
    if rec["key"] != json!(key(entry, exe, opts)) || rec["output"] != output_stamp(exe) {
        return false;
    }
    let Some(files) = rec["files"].as_array() else {
        return false;
    };
    files.iter().all(|f| {
        let path = Path::new(f[0].as_str().unwrap_or(""));
        f[1] == json!(file_hash(path))
    })
}

/// Records what `exe` was just built from (`checked` is the check it was built from).
pub fn record(checked: &Checked, entry: &Path, exe: &Path, opts: &BuildOptions) {
    let path = record_path(exe);
    let untracked = checked.sm.ids().any(|f| {
        let n = &checked.sm.file(f).name;
        n.starts_with("<c:") || n.starts_with("<js:")
    });
    if !tracked(opts) || untracked {
        let _ = std::fs::remove_file(&path);
        return;
    }
    // The sources by the text the check compiled (an edit during the build isn't "built"); the
    // entry is the first file, by the path it was built from.
    let mut files: Vec<(PathBuf, String)> = Vec::new();
    for (i, f) in checked.sm.ids().enumerate() {
        let file = checked.sm.file(f);
        // (Embedded and generated files, `<core>/…`, come with the compiler; module paths may
        // be relative to the current directory, as the entry was given.)
        let p = if i == 0 {
            canonical(entry)
        } else if file.name.starts_with('<') {
            continue;
        } else {
            canonical(Path::new(&file.name))
        };
        files.push((p, text_hash(file.text.as_bytes())));
    }
    let mut others: Vec<PathBuf> = checked.files_read.clone();
    // Each `project.json` from the entry's directory up, and where there's none (creating one
    // changes the configuration).
    if let Some(dir) = canonical(entry).parent() {
        others.extend(dir.ancestors().map(|d| d.join("project.json")));
    }
    files.extend(others.into_iter().map(|p| {
        let h = file_hash(&p);
        (p, h)
    }));
    files.sort();
    files.dedup_by(|a, b| a.0 == b.0);
    let files: Vec<Value> = files
        .iter()
        .map(|(p, h)| json!([p.display().to_string(), h]))
        .collect();
    let rec = json!({
        "key": key(entry, exe, opts),
        "files": files,
        "output": output_stamp(exe),
    });
    if let Some(d) = path.parent()
        && std::fs::create_dir_all(d).is_ok()
    {
        // (Written whole, then renamed: a crash leaves no half record.)
        let tmp = path.with_extension("tmp");
        if std::fs::write(&tmp, rec.to_string()).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

/// Forgets `exe`'s fingerprint (its next build builds).
pub fn forget(exe: &Path) {
    let _ = std::fs::remove_file(record_path(exe));
}

fn tracked(opts: &BuildOptions) -> bool {
    opts.target.unwrap_or_else(Target::host).os != target::Os::Browser
}

/// What must be the same for a fingerprint to apply: the compiler, the options, the
/// environment the build reads, the entry and the output.
fn key(entry: &Path, exe: &Path, opts: &BuildOptions) -> String {
    let compiler = std::env::current_exe()
        .ok()
        .and_then(|p| {
            let m = std::fs::metadata(&p).ok()?;
            Some(format!(
                "{} {} {:?}",
                p.display(),
                m.len(),
                m.modified().ok()?
            ))
        })
        .unwrap_or_default();
    let env: Vec<String> = ["PICO_BACKEND", "PICO_STDLIB"]
        .iter()
        .map(|v| format!("{v}={}", std::env::var(v).unwrap_or_default()))
        .collect();
    format!(
        "{} | {compiler} | release={} target={} | {} | {} -> {}",
        env!("CARGO_PKG_VERSION"),
        opts.release,
        opts.target.unwrap_or_else(Target::host),
        env.join(" "),
        canonical(entry).display(),
        canonical_output(exe).display(),
    )
}

fn canonical(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
}

/// A file's content hash, or `"absent"`.
fn file_hash(p: &Path) -> String {
    match std::fs::read(p) {
        Ok(bytes) => text_hash(&bytes),
        Err(_) => "absent".into(),
    }
}

fn text_hash(bytes: &[u8]) -> String {
    let mut h = DefaultHasher::new();
    h.write(bytes);
    format!("{:016x}", h.finish())
}

/// The output's size and modification time (`null` if it's missing).
fn output_stamp(exe: &Path) -> Value {
    match std::fs::metadata(exe) {
        Ok(m) => json!([m.len(), format!("{:?}", m.modified().ok())]),
        Err(_) => Value::Null,
    }
}

/// Where `exe`'s fingerprint is kept: the user's cache directory, by the output's path.
fn record_path(exe: &Path) -> PathBuf {
    let mut h = DefaultHasher::new();
    h.write(canonical_output(exe).display().to_string().as_bytes());
    cache_dir()
        .join("pico")
        .join("builds")
        .join(format!("{:016x}.json", h.finish()))
}

/// The output's absolute path (it may not exist yet: its directory's, joined).
fn canonical_output(exe: &Path) -> PathBuf {
    if let Ok(p) = exe.canonicalize() {
        return p;
    }
    let dir = exe
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    canonical(dir).join(exe.file_name().unwrap_or_default())
}

/// The user's cache directory (`PICO_CACHE_DIR` overrides it).
pub fn cache_dir() -> PathBuf {
    let env = |v: &str| {
        std::env::var_os(v)
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
    };
    if let Some(d) = env("PICO_CACHE_DIR") {
        return d;
    }
    if cfg!(windows) {
        if let Some(d) = env("LOCALAPPDATA") {
            return d;
        }
    } else if cfg!(target_os = "macos") {
        if let Some(h) = env("HOME") {
            return h.join("Library/Caches");
        }
    } else if let Some(d) = env("XDG_CACHE_HOME") {
        return d;
    } else if let Some(h) = env("HOME") {
        return h.join(".cache");
    }
    std::env::temp_dir()
}
