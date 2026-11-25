//! `pico run --target` (#77): builds for the target and runs the program here: a musl program
//! of this machine's architecture directly; a macOS program can't run on Linux, which says so;
//! `--interp` takes no target.
#![cfg(target_os = "linux")]

use std::process::Command;

fn pico(dir: &std::path::Path, args: &[&str]) -> (bool, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_pico"))
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn run_builds_for_the_target_and_runs_it_here() {
    let d = std::env::temp_dir().join(format!("pico-run-target-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(
        d.join("main.pi"),
        "import { args } from \"std/process\"\nprintln(`hi ${args.join(\",\")}`)\n",
    )
    .unwrap();

    let musl = if cfg!(target_arch = "x86_64") {
        "linux-x64-musl"
    } else {
        "linux-arm64-musl"
    };
    let (ok, out, err) = pico(&d, &["run", "--target", musl, "main.pi", "--", "a", "b"]);
    assert!(ok, "{err}");
    assert_eq!(out, "hi a,b\n");

    let (ok, _, err) = pico(&d, &["run", "--target", "macos-arm64", "main.pi"]);
    assert!(!ok);
    assert!(err.contains("can't run on this machine"), "{err}");

    let (ok, _, err) = pico(&d, &["run", "--interp", "--target", musl, "main.pi"]);
    assert!(!ok);
    assert!(err.contains("takes no `--target`"), "{err}");
    let _ = std::fs::remove_dir_all(&d);
}
