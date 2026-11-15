//! `pico lint` stays fast (#143): linting the standard library has a CPU-time budget, and two
//! runs report the same thing (files are linted in parallel; the report is in file order).
//!
//! Linting used to lower every file's program to MIR: 9.4 s on the standard library with a
//! release build. Now it stops after type checking. The budget is on CPU time, not wall-clock
//! time: that doesn't depend on how many cores the runner has (parallel linting would hide a
//! return to lowering on a big machine) or on what else is running. That lint skips MIR is
//! also checked directly, in pico-lint's `signature_errors_without_lowering`.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// The CPU time `pico lint stdlib` may use, user and system, all threads. With this test's
/// debug build on linux-arm64 it uses 14–18 s, and used 86 s when lint lowered to MIR: the
/// budget is over twice the first and under half the second.
const BUDGET: Duration = Duration::from_secs(40);

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The CPU time this process's waited-for children have used so far.
fn children_cpu() -> Duration {
    let mut u: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: `u` is a valid `rusage` to fill in.
    let rc = unsafe { libc::getrusage(libc::RUSAGE_CHILDREN, &mut u) };
    assert_eq!(rc, 0, "getrusage failed");
    let t = |tv: libc::timeval| {
        Duration::from_secs(tv.tv_sec as u64) + Duration::from_micros(tv.tv_usec as u64)
    };
    t(u.ru_utime) + t(u.ru_stime)
}

/// `pico lint stdlib`'s report, and the CPU time it used.
fn lint_stdlib() -> (String, Duration) {
    let before = children_cpu();
    let out = Command::new(env!("CARGO_BIN_EXE_pico"))
        .current_dir(repo())
        .args(["lint", "stdlib"])
        .output()
        .expect("run pico lint");
    let cpu = children_cpu() - before;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (text, cpu)
}

#[test]
fn linting_the_stdlib_is_fast_and_deterministic() {
    let (first, cpu) = lint_stdlib();
    eprintln!("pico lint stdlib: {cpu:?} of CPU time");
    assert!(first.contains(" file(s)"), "no summary line:\n{first}");
    assert!(
        cpu < BUDGET,
        "`pico lint stdlib` used {cpu:?} of CPU time, over its budget of {BUDGET:?} (#143)"
    );
    let (second, _) = lint_stdlib();
    assert_eq!(first, second, "two runs of `pico lint stdlib` differ");
}
