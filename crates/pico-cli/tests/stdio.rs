//! A native program's own stdout is delivered as soon as `stdout.write` settles, even when it's a
//! pipe (#82): a parent can hold a request/response conversation over the child's stdio.
//! (Golden tests only see output after exit, so this needs a live parent.)

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

/// A scratch directory for one test.
fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("pico-stdio-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Builds `src` natively and returns the executable.
fn build(d: &std::path::Path, src: &str) -> PathBuf {
    std::fs::write(d.join("main.pi"), src).unwrap();
    let exe = d.join(format!("main{}", std::env::consts::EXE_SUFFIX));
    let out = Command::new(env!("CARGO_BIN_EXE_pico"))
        .arg("build")
        .arg(d.join("main.pi"))
        .arg("-o")
        .arg(&exe)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "pico build failed:\n{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    exe
}

#[test]
fn stdout_writes_reach_a_pipe_before_exit() {
    let d = scratch("ping");
    let exe = build(
        &d,
        r#"import { stdin, stdout } from "std/process"

for await (const chunk of stdin) {
  await stdout.write(chunk)
  await stdout.writeText("!\n")
}
"#,
    );
    let mut child = Command::new(&exe)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();

    // Bytes from the child's stdout, as they arrive.
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut buf = [0u8; 256];
        while let Ok(n) = stdout.read(&mut buf) {
            if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                break;
            }
        }
    });
    let reply = |want: &str| {
        let mut got = Vec::new();
        while got.len() < want.len() {
            match rx.recv_timeout(Duration::from_secs(30)) {
                Ok(bytes) => got.extend(bytes),
                Err(_) => panic!(
                    "no reply while stdin is still open: got {:?}, want {want:?}",
                    String::from_utf8_lossy(&got)
                ),
            }
        }
        assert_eq!(String::from_utf8_lossy(&got), want);
    };

    // Two round trips with stdin still open: each reply must arrive before the next request.
    stdin.write_all(b"ping\n").unwrap();
    stdin.flush().unwrap();
    reply("ping\n!\n");
    stdin.write_all(b"pong\n").unwrap();
    stdin.flush().unwrap();
    reply("pong\n!\n");

    drop(stdin);
    assert!(child.wait().unwrap().success());
    let _ = std::fs::remove_dir_all(&d);
}
