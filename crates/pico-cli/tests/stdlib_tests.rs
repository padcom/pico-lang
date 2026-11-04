//! The standard library's own tests (`stdlib/tests/*.test.pi`), run through `pico test`
//! natively and in the interpreter, and a check that failures and benchmarks are reported.

use std::path::{Path, PathBuf};
use std::process::Command;

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn pico_test(args: &[&str]) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_pico"))
        .current_dir(repo())
        .args(args)
        .output()
        .expect("run pico");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), text)
}

#[test]
fn stdlib_tests_pass_natively() {
    let (ok, out) = pico_test(&["test", "stdlib/tests"]);
    assert!(ok, "{out}");
    assert!(out.contains(" 0 failed, 0 skipped"), "{out}");
}

#[test]
fn stdlib_tests_pass_interpreted() {
    let (ok, out) = pico_test(&["test", "--interp", "stdlib/tests"]);
    assert!(ok, "{out}");
}

#[test]
fn failures_and_filters_are_reported() {
    let dir = std::env::temp_dir().join(format!("pico-test-cli-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("a.test.pi"),
        "import { test, expect } from \"std/test\"\n\
         test(\"good\", () => expect(1 + 1).toBe(2))\n\
         test(\"bad\", () => expect(\"x\").toBe(\"y\"))\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("b.bench.pi"),
        "import { bench } from \"std/test\"\nbench(\"noop\", () => {})\n",
    )
    .unwrap();
    let d = dir.to_string_lossy().into_owned();
    let (ok, out) = pico_test(&["test", "--interp", &d]);
    assert!(!ok, "{out}");
    assert!(
        out.contains("✗ bad") && out.contains("expected \"x\" to be \"y\""),
        "{out}"
    );
    assert!(out.contains("tests: 1 passed, 1 failed"), "{out}");
    let (ok, out) = pico_test(&["test", "--interp", "-t", "good", &d]);
    assert!(ok, "{out}");
    let (ok, out) = pico_test(&["bench", &d]);
    assert!(ok, "{out}");
    assert!(out.contains("noop: mean"), "{out}");
    let _ = std::fs::remove_dir_all(&dir);
}
