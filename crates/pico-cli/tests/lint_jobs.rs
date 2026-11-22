//! `pico lint -j N` (#166): any number of workers gives the same report, in file order, and
//! `-j 0` still lints (one worker).

use std::path::{Path, PathBuf};
use std::process::Command;

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn lint(jobs: &str) -> (String, bool) {
    let out = Command::new(env!("CARGO_BIN_EXE_pico"))
        .current_dir(repo())
        .args([
            "lint",
            "-j",
            jobs,
            "stdlib/path.pi",
            "stdlib/fmt.pi",
            "stdlib/time.pi",
        ])
        .output()
        .expect("run pico lint");
    let text =
        String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
    (text, out.status.success())
}

#[test]
fn any_number_of_workers_reports_the_same() {
    let one = lint("1");
    assert!(one.1, "pico lint -j 1 failed:\n{}", one.0);
    assert!(one.0.contains("in 3 file(s)"), "{}", one.0);
    assert_eq!(lint("4"), one);
    assert_eq!(lint("0"), one);
}
