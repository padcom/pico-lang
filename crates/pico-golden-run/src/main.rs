//! Runs the golden-test executables that `pico-tests` exported (`PICO_TEST_EXPORT`) and checks
//! them against their `.expect` files, the way the harness checks native runs: stdout line by
//! line, the exit code, and the panic message (the first line of stderr, after `panic: `).
//! Each program runs in a fresh empty working directory. Used where the compiler can't run,
//! like the Windows CI runners.
//!
//!     pico-golden-run <dir>

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

/// A program running longer than this fails (some fixtures use timers and workers).
const TIMEOUT: Duration = Duration::from_secs(60);

/// The environment variable every golden run gets: the same as `pico_tests::TEST_ENV` (this crate
/// has no dependencies, so it's repeated here).
const TEST_ENV: (&str, &str) = ("PICO_TEST_ENV", "1");

struct Expect {
    exit: i32,
    panic: Option<String>,
    stdout: Vec<String>,
}

fn parse(text: &str) -> Option<Expect> {
    let mut lines = text.split('\n');
    let exit = lines.next()?.strip_prefix("exit ")?.parse().ok()?;
    let panic = match lines.next()? {
        "panic" => None,
        l => Some(l.strip_prefix("panic: ")?.to_string()),
    };
    let mut stdout: Vec<String> = lines.map(str::to_string).collect();
    if stdout.last().is_some_and(String::is_empty) {
        stdout.pop();
    }
    Some(Expect {
        exit,
        panic,
        stdout,
    })
}

/// Runs `exe` in `cwd`, killing it after [`TIMEOUT`].
fn run(exe: &Path, cwd: &Path) -> Result<(i32, String, String), String> {
    let mut child = Command::new(exe)
        .current_dir(cwd)
        .env(TEST_ENV.0, TEST_ENV.1)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run: {e}"))?;
    // Read the pipes on threads, so a chatty program can't block on a full pipe.
    let mut out = child.stdout.take().unwrap();
    let mut err = child.stderr.take().unwrap();
    let t_out = std::thread::spawn(move || {
        let mut v = Vec::new();
        let _ = std::io::Read::read_to_end(&mut out, &mut v);
        v
    });
    let t_err = std::thread::spawn(move || {
        let mut v = Vec::new();
        let _ = std::io::Read::read_to_end(&mut err, &mut v);
        v
    });
    let start = Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait().map_err(|e| e.to_string())? {
            break s;
        }
        if start.elapsed() > TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("timed out after {}s", TIMEOUT.as_secs()));
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let stdout = String::from_utf8_lossy(&t_out.join().unwrap()).into_owned();
    let stderr = String::from_utf8_lossy(&t_err.join().unwrap()).into_owned();
    Ok((status.code().unwrap_or(-1), stdout, stderr))
}

fn check(exe: &Path, want: &Expect, cwd: &Path) -> Result<(), String> {
    let (code, stdout, stderr) = run(exe, cwd)?;
    let mut problems = Vec::new();
    let mut got: Vec<&str> = stdout.split('\n').collect();
    if got.last() == Some(&"") {
        got.pop();
    }
    if got != want.stdout {
        let mut d = String::from("stdout differs (- expected, + actual):\n");
        for l in &want.stdout {
            d.push_str(&format!("  - {l}\n"));
        }
        for l in &got {
            d.push_str(&format!("  + {l}\n"));
        }
        problems.push(d);
    }
    if code != want.exit {
        problems.push(format!("exit code: expected {}, got {code}", want.exit));
    }
    let panic = stderr
        .lines()
        .next()
        .and_then(|l| l.strip_prefix("panic: "))
        .filter(|_| code == 101);
    match (&want.panic, panic) {
        (Some(w), Some(g)) if w != g => {
            problems.push(format!("panic message: expected {w:?}, got {g:?}"))
        }
        (Some(w), None) => problems.push(format!("expected a panic ({w:?})")),
        (None, Some(g)) => problems.push(format!("unexpected panic: {g}")),
        _ => {}
    }
    if problems.is_empty() {
        Ok(())
    } else {
        if !stderr.is_empty() {
            problems.push(format!("stderr:\n{}", stderr.trim_end()));
        }
        Err(problems.join("\n"))
    }
}

fn main() -> ExitCode {
    let Some(dir) = std::env::args_os().nth(1).map(PathBuf::from) else {
        eprintln!("usage: pico-golden-run <dir>");
        return ExitCode::from(2);
    };
    // (Programs run in other directories, so their paths must be absolute.)
    let dir = std::fs::canonicalize(&dir).unwrap_or(dir);
    let mut cases: Vec<PathBuf> = match std::fs::read_dir(&dir) {
        Ok(rd) => rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "expect"))
            .collect(),
        Err(e) => {
            eprintln!("cannot read {}: {e}", dir.display());
            return ExitCode::from(2);
        }
    };
    cases.sort();
    let work = std::env::temp_dir().join(format!("pico-golden-run-{}", std::process::id()));
    let (mut passed, mut failed) = (0, Vec::new());
    for (i, case) in cases.iter().enumerate() {
        let name = case
            .file_stem()
            .unwrap()
            .to_string_lossy()
            .replace("__", "/");
        let exe = ["exe", ""]
            .iter()
            .map(|x| case.with_extension(x))
            .find(|p| p.is_file());
        let want = std::fs::read_to_string(case).ok().and_then(|t| parse(&t));
        let cwd = work.join(i.to_string());
        let _ = std::fs::create_dir_all(&cwd);
        let r = match (exe, want) {
            (Some(exe), Some(want)) => check(&exe, &want, &cwd),
            (None, _) => Err("no executable".into()),
            (_, None) => Err("malformed .expect".into()),
        };
        let _ = std::fs::remove_dir_all(&cwd);
        match r {
            Ok(()) => passed += 1,
            Err(e) => failed.push((name, e)),
        }
    }
    let _ = std::fs::remove_dir_all(&work);
    for (name, e) in &failed {
        println!("──── FAIL {name}\n{e}\n");
    }
    println!("golden runs: {passed} passed, {} failed", failed.len());
    if failed.is_empty() && passed > 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
