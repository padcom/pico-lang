//! A Unix-domain listener's socket file goes when the program ends without closing it (#85): at
//! the end of the program, at `exit()`, at a panic and, natively, at SIGTERM. Both backends
//! (signals natively only: the interpreter shares its process with `pico`).
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("pico-unix-cleanup-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Runs `body` (after a listener on `s.sock` is open) and checks the file is gone afterwards.
fn ends_without_file(name: &str, body: &str, interp: bool) {
    let d = scratch(&format!("{name}-{interp}"));
    let src = format!(
        "import {{ listenUnix }} from \"std/net\"\nimport {{ exit }} from \"std/process\"\nconst l = await listenUnix(\"s.sock\")\nprintln(\"listening\")\n{body}\n"
    );
    std::fs::write(d.join("main.pi"), src).unwrap();
    let mut c = Command::new(env!("CARGO_BIN_EXE_pico"));
    c.arg("run");
    if interp {
        c.arg("--interp");
    }
    let out = c.arg("main.pi").current_dir(&d).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("listening"),
        "{name}: {stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !d.join("s.sock").exists(),
        "{name} (interp={interp}): the socket file is still there"
    );
}

#[test]
fn files_go_at_exit_and_panics() {
    for interp in [false, true] {
        ends_without_file("end", "", interp);
        ends_without_file("exit", "exit(3)", interp);
        ends_without_file("panic", "panic(\"boom\")", interp);
    }
}

#[test]
fn files_go_at_sigterm() {
    let d = scratch("sigterm");
    std::fs::write(
        d.join("main.pi"),
        "import { listenUnix } from \"std/net\"\nimport { delay } from \"std/timers\"\nconst l = await listenUnix(\"s.sock\")\nprintln(\"listening\")\nawait delay(60000)\n",
    )
    .unwrap();
    let built = Command::new(env!("CARGO_BIN_EXE_pico"))
        .args(["build", "main.pi", "-o", "main"])
        .current_dir(&d)
        .status()
        .unwrap();
    assert!(built.success());
    let mut child = Command::new(d.join("main"))
        .current_dir(&d)
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    // (Its output to a pipe is buffered: wait for the socket file itself.)
    let mut up = false;
    for _ in 0..500 {
        if sock(&d).exists() {
            up = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(up, "the listener never came up");
    let _ = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status();
    let st = child.wait().unwrap();
    // Ended by the signal itself (re-raised after the cleanup), not by an exit code.
    assert!(
        std::os::unix::process::ExitStatusExt::signal(&st) == Some(15),
        "{st:?}"
    );
    for _ in 0..100 {
        if !sock(&d).exists() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    panic!("the socket file is still there after SIGTERM");
}

fn sock(d: &Path) -> PathBuf {
    d.join("s.sock")
}
