//! pico's lldb formatters (`src/pico_lldb.py`) on a real program, through plain `lldb` in
//! batch mode: reactive cells show their values (spec §16, PLAN P11.5). Skipped where there's
//! no lldb (`PICO_LLDB`, `lldb` on PATH, or Xcode's).

use std::process::{Command, Stdio};

fn lldb() -> Option<String> {
    if let Some(p) = std::env::var_os("PICO_LLDB") {
        return Some(p.to_string_lossy().into_owned());
    }
    let works = |exe: &str| {
        Command::new(exe)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    };
    for exe in ["lldb", "lldb-23", "lldb-22", "lldb-21"] {
        if works(exe) {
            return Some(exe.into());
        }
    }
    let out = Command::new("xcrun").args(["-f", "lldb"]).output().ok()?;
    let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && works(&p)).then_some(p)
}

#[test]
fn reactive_values() {
    let Some(lldb) = lldb() else {
        eprintln!("note: no lldb: skipping");
        return;
    };
    let dir = std::env::temp_dir().join(format!("pico-lldb-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let main = dir.join("main.pi");
    std::fs::write(
        &main,
        "function show(count: Reactive<int>, doubled: Reactive<int>, name: Reactive<string>, lazy: Reactive<int>) {\n  println(`${count} ${name}`)\n}\nreactive count = 3\nreactive name = \"Ann\"\nconst doubled = computed(() => count * 2)\nconst lazy = computed(() => count + 1)\nprintln(doubled)\ncount = 4\nshow(count, doubled, name, lazy)\n",
    )
    .unwrap();
    let exe = dir.join("main");
    // (dsymutil, which keeps the debug info on macOS, needs `TMPDIR` to exist.)
    std::fs::create_dir_all(dir.join("cache")).unwrap();
    let built = Command::new(env!("CARGO_BIN_EXE_pico"))
        .arg("build")
        .arg(&main)
        .arg("-o")
        .arg(&exe)
        .env("TMPDIR", dir.join("cache"))
        .output()
        .unwrap();
    assert!(
        built.status.success(),
        "{}",
        String::from_utf8_lossy(&built.stderr)
    );
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/src/pico_lldb.py");
    let out = Command::new(&lldb)
        .arg("-b")
        .args(["-o", &format!("command script import {script}")])
        // (By name: the debug info has the canonical path, `/private/var/…` on macOS.)
        .args(["-o", "breakpoint set -f main.pi -l 2"])
        .args([
            "-o",
            "run",
            "-o",
            "frame variable",
            "-o",
            "frame variable count.value",
        ])
        .arg(&exe)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    for want in [
        "(Reactive<i64>) count = 4",
        "(Reactive<i64>) doubled = 6 (dirty)",
        "(Reactive<string>) name = \"Ann\"",
        "(Reactive<i64>) lazy = not computed yet",
        "value = 4",
    ] {
        assert!(text.contains(want), "{want}:\n{text}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}
