//! `pico symbolize` (spec §7.1, PLAN P10.8d): a release build's panic trace (offsets into the
//! executable, a build ID) resolved from the debug file the build wrote. Panic traces exist
//! where `backtrace` does (glibc, macOS).
#![cfg(any(all(target_os = "linux", target_env = "gnu"), target_os = "macos"))]

use std::process::Command;

fn pico(args: &[&str], dir: &std::path::Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_pico"))
        .args(args)
        .current_dir(dir)
        .env("PICO_CACHE_DIR", dir.join("cache"))
        .output()
        .unwrap()
}

#[test]
fn release_traces() {
    let dir = std::env::temp_dir().join(format!("pico-symbolize-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let main = "function inner(xs: i64[]): i64 { xs[5] }\nfunction outer(): i64 { inner([1, 2]) }\nprintln(outer())\n";
    std::fs::write(dir.join("main.pi"), main).unwrap();
    let b = pico(&["build", "--release", "main.pi", "-o", "app"], &dir);
    assert!(b.status.success(), "{}", String::from_utf8_lossy(&b.stderr));
    let run = Command::new(dir.join("app")).output().unwrap();
    let trace = String::from_utf8_lossy(&run.stderr).to_string();
    // The release trace: offsets under a build ID, no names.
    assert!(trace.contains("stack trace (build "), "{trace}");
    assert!(trace.contains(": app+0x"), "{trace}");
    assert!(!trace.contains("inner"), "{trace}");
    std::fs::write(dir.join("trace.txt"), &trace).unwrap();
    let s = pico(&["symbolize", "trace.txt"], &dir);
    let out = String::from_utf8_lossy(&s.stdout);
    assert!(
        s.status.success(),
        "{out}{}",
        String::from_utf8_lossy(&s.stderr)
    );
    // As a debug build's, with locations (`inner` and `outer` may be inlined: frames anyway).
    assert!(out.contains("panic: index out of bounds"), "{out}");
    assert!(out.contains("stack trace:\n"), "{out}");
    assert!(out.contains("0: inner at main.pi:1:"), "{out}");
    assert!(out.contains("1: outer at main.pi:2:"), "{out}");
    assert!(out.contains("2: <main> at main.pi:3:"), "{out}");

    // Another build's debug file doesn't fit the trace.
    std::fs::write(dir.join("main.pi"), main.replace("xs[5]", "xs[6]")).unwrap();
    let b = pico(&["build", "--release", "main.pi", "-o", "app"], &dir);
    assert!(b.status.success());
    let s = pico(&["symbolize", "trace.txt"], &dir);
    assert!(!s.status.success());
    assert!(
        String::from_utf8_lossy(&s.stderr).contains("another build"),
        "{}",
        String::from_utf8_lossy(&s.stderr)
    );
}
