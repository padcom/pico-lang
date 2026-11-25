//! `pico build` writes `<output>.notices.txt` (#106): pico's license always, and only the
//! third-party components the linker took in (Mbed TLS with `std/crypto`, musl on musl targets,
//! mingw-w64 on Windows).
#![cfg(target_os = "linux")]

use std::path::Path;
use std::process::Command;

fn build(dir: &Path, src: &str, out: &str, target: Option<&str>) -> String {
    std::fs::write(dir.join(format!("{out}.pi")), src).unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_pico"));
    cmd.arg("build");
    if let Some(t) = target {
        cmd.args(["--target", t]);
    }
    let run = cmd
        .arg(format!("{out}.pi"))
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        run.status.success(),
        "pico build {out}: {}",
        String::from_utf8_lossy(&run.stderr)
    );
    // (No `-o`: the output is named after the file, on Windows too.)
    let notices = dir.join(format!("{out}.notices.txt"));
    assert!(
        !dir.join(format!("{out}.notices.map")).exists(),
        "the map is removed"
    );
    std::fs::read_to_string(&notices).unwrap_or_else(|e| panic!("{}: {e}", notices.display()))
}

#[test]
fn notices_list_what_is_linked_in() {
    let d = std::env::temp_dir().join(format!("pico-notices-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();

    let hello = build(&d, "println(\"hi\")\n", "hello", None);
    assert!(hello.contains("- Pico (the runtime and the standard library), MIT\n"));
    assert!(hello.contains("Permission is hereby granted"), "{hello}");
    assert!(!hello.contains("Mbed TLS"), "{hello}");
    assert!(!hello.contains("musl"), "{hello}");

    let crypto = build(
        &d,
        "import { digest } from \"std/crypto\"\nprintln(digest(\"SHA-256\", \"abc\").length)\n",
        "hash",
        None,
    );
    assert!(crypto.contains("- Mbed TLS 3.6"), "{crypto}");
    assert!(crypto.contains("Apache License"), "{crypto}");

    let musl = build(&d, "println(\"hi\")\n", "static", Some("linux-arm64-musl"));
    assert!(musl.contains("- musl libc, MIT\n"), "{musl}");
    assert!(!musl.contains("Mbed TLS"), "{musl}");

    let win = build(&d, "println(\"hi\")\n", "win", Some("windows-x64"));
    assert!(win.contains("- mingw-w64"), "{win}");
    assert!(win.contains("Zope Public License"), "{win}");

    // Up to date, but the notices file is gone: it's written again.
    std::fs::remove_file(d.join("hello.notices.txt")).unwrap();
    let again = build(&d, "println(\"hi\")\n", "hello", None);
    assert_eq!(again, hello);
    let _ = std::fs::remove_dir_all(&d);
}
