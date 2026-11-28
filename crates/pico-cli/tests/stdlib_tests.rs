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

/// `toMatchSnapshot` (#70): the first run stores the snapshots next to the test file, a later
/// run compares (a change fails), and `pico test -u` stores them again.
#[test]
fn snapshots_are_stored_compared_and_updated() {
    let dir = std::env::temp_dir().join(format!("pico-test-snap-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("s.test.pi");
    let write = |n: i32| {
        std::fs::write(
            &file,
            format!(
                "import {{ test, expect }} from \"std/test\"\n\
                 test(\"values\", () => {{\n\
                 expect([1, 2]).toMatchSnapshot()\n\
                 expect(\"two\\nlines\").toMatchSnapshot()\n\
                 expect({n}).toMatchSnapshot()\n\
                 }})\n"
            ),
        )
        .unwrap();
    };
    write(1);
    let f = file.to_string_lossy().into_owned();
    let (ok, out) = pico_test(&["test", "--interp", &f]);
    assert!(ok, "{out}");
    let snap = dir.join("__snapshots__").join("s.test.pi.snap");
    let text = std::fs::read_to_string(&snap).expect("a snapshot file");
    assert!(
        text.contains("[values 1]\n  [1, 2]") && text.contains("[values 2]\n  two\n  lines"),
        "{text}"
    );
    let (ok, out) = pico_test(&["test", "--interp", &f]);
    assert!(ok, "{out}");
    write(2);
    let (ok, out) = pico_test(&["test", "--interp", &f]);
    assert!(
        !ok && out.contains("snapshot `values 3` doesn't match"),
        "{out}"
    );
    let (ok, out) = pico_test(&["test", "--interp", "-u", &f]);
    assert!(ok, "{out}");
    assert!(
        std::fs::read_to_string(&snap)
            .unwrap()
            .contains("[values 3]\n  2")
    );
    let (ok, out) = pico_test(&["test", "--interp", &f]);
    assert!(ok, "{out}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// `pico test --watch` re-runs only the test files a change affects: those that are, or
/// import, a changed file (#70).
#[test]
fn watch_reruns_only_affected_files() {
    use std::io::{BufRead, BufReader};
    use std::sync::mpsc;
    use std::time::Duration;
    let dir = std::env::temp_dir().join(format!("pico-test-watch-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let helper = "export function one(): int { return 1 }\n";
    std::fs::write(dir.join("helper.pi"), helper).unwrap();
    std::fs::write(
        dir.join("other.pi"),
        "export function two(): int { return 2 }\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("a.test.pi"),
        "import { test, expect } from \"std/test\"\nimport { one } from \"./helper\"\n\
         test(\"a\", () => expect(one()).toBe(1))\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("b.test.pi"),
        "import { test, expect } from \"std/test\"\ntest(\"b\", () => expect(2).toBe(2))\n",
    )
    .unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_pico"))
        .current_dir(repo())
        .args(["test", "--interp", "--watch"])
        .arg(&dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("run pico");
    let (tx, rx) = mpsc::channel();
    let stdout = child.stdout.take().unwrap();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    // The lines up to the next "watching for changes".
    let round = || {
        let mut lines = Vec::new();
        loop {
            let line = rx
                .recv_timeout(Duration::from_secs(180))
                .unwrap_or_else(|_| panic!("no `watching` line; got:\n{}", lines.join("\n")));
            if line.starts_with("watching for changes") {
                return lines.join("\n");
            }
            lines.push(line);
        }
    };
    let first = round();
    // Each change gets a new modification time even on a coarse clock.
    let touch = |name: &str, text: &str| {
        std::thread::sleep(Duration::from_millis(1100));
        std::fs::write(dir.join(name), text).unwrap();
    };
    touch("helper.pi", &format!("{helper}// changed\n"));
    let second = round();
    touch("other.pi", "export function two(): int { return 3 }\n");
    let third = round();
    child.kill().ok();
    child.wait().ok();
    std::fs::remove_dir_all(&dir).ok();
    assert!(
        first.contains("a.test.pi") && first.contains("b.test.pi"),
        "{first}"
    );
    assert!(
        second.contains("✓") && second.contains("a.test.pi") && !second.contains("b.test.pi"),
        "{second}"
    );
    assert!(
        third.contains("no test file depends on the change"),
        "{third}"
    );
}
