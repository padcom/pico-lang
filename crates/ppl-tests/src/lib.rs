//! Golden-test harness for ppl.
//!
//! Fixtures live in `tests/run/**/*.ppl` (programs that must compile and run) and
//! `tests/fail/**/*.ppl` (programs that must fail to compile). Expectations are comment
//! directives:
//!
//! | Directive | Meaning |
//! |-----------|---------|
//! | `// expect: text` | one line of expected stdout, in order (`run` only) |
//! | `// exit: N` | expected exit code (default 0, or 101 with `panic`) |
//! | `// panic: msg` | the program must panic with this message |
//! | `// error: E0308` | an expected compile error, in order (`fail` only) |
//! | `// at: 4:19` | position (line:col) of the preceding `error` |
//!
//! A directory containing `main.ppl` is one multi-file fixture; its other files are modules.
//!
//! The harness runs every stage that exists. Checks that need a missing stage are reported
//! as *pending* rather than passing silently.

use ppl_diag::{Code, codes};
use ppl_driver::Stage;
use std::fmt::Write;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Run,
    Fail,
}

#[derive(Debug, PartialEq)]
pub struct ExpectedError {
    pub code: Code,
    pub at: Option<(usize, usize)>,
    /// Line of the `// error:` directive, for messages.
    pub line: usize,
}

#[derive(Debug, Default, PartialEq)]
pub struct Directives {
    pub expect: Vec<String>,
    pub exit: Option<i32>,
    pub panic: Option<String>,
    pub errors: Vec<ExpectedError>,
}

/// Parses the directives in a fixture. Lines that look like directives but are malformed are
/// errors, so typos don't silently disable a check.
pub fn parse_directives(text: &str) -> Result<Directives, Vec<String>> {
    let mut d = Directives::default();
    let mut problems = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let n = i + 1;
        let Some(rest) = line.trim_start().strip_prefix("//") else {
            continue;
        };
        let rest = rest.strip_prefix(' ').unwrap_or(rest);
        let Some((name, value)) = rest.split_once(':') else {
            continue;
        };
        if !matches!(name, "expect" | "exit" | "panic" | "error" | "at") {
            continue;
        }
        // `// expect: text` keeps everything after one space; others are trimmed.
        let raw = value.strip_prefix(' ').unwrap_or(value);
        let value = value.trim();
        match name {
            "expect" => d.expect.push(raw.to_string()),
            "exit" => match value.parse() {
                Ok(code) if d.exit.is_none() => d.exit = Some(code),
                Ok(_) => problems.push(format!("line {n}: more than one `exit` directive")),
                Err(_) => problems.push(format!("line {n}: `exit` needs a number, got {value:?}")),
            },
            "panic" => {
                if d.panic.replace(value.to_string()).is_some() {
                    problems.push(format!("line {n}: more than one `panic` directive"));
                }
            }
            "error" => match parse_code(value) {
                Some(code) => d.errors.push(ExpectedError {
                    code,
                    at: None,
                    line: n,
                }),
                None => problems.push(format!(
                    "line {n}: `error` needs a code like E0308, got {value:?}"
                )),
            },
            "at" => {
                let pos = value
                    .split_once(':')
                    .and_then(|(l, c)| Some((l.trim().parse().ok()?, c.trim().parse().ok()?)));
                match (pos, d.errors.last_mut()) {
                    (Some(p), Some(e)) if e.at.is_none() => e.at = Some(p),
                    (Some(_), _) => {
                        problems.push(format!("line {n}: `at` must follow an `error` directive"))
                    }
                    (None, _) => {
                        problems.push(format!("line {n}: `at` needs line:col, got {value:?}"))
                    }
                }
            }
            _ => unreachable!(),
        }
    }
    if problems.is_empty() {
        Ok(d)
    } else {
        Err(problems)
    }
}

fn parse_code(s: &str) -> Option<Code> {
    let digits = s.strip_prefix('E')?;
    if digits.len() != 4 {
        return None;
    }
    digits.parse().ok().map(Code)
}

#[derive(Debug)]
pub struct Fixture {
    /// The `.ppl` file (or `main.ppl` of a multi-file fixture).
    pub path: PathBuf,
    /// Path relative to the tests root, for display (`run/hello.ppl`).
    pub name: String,
    pub kind: Kind,
}

/// Finds every fixture under `root/run` and `root/fail`, sorted by name.
pub fn discover(root: &Path) -> Vec<Fixture> {
    let mut out = Vec::new();
    for (sub, kind) in [("run", Kind::Run), ("fail", Kind::Fail)] {
        let dir = root.join(sub);
        if dir.is_dir() {
            walk(&dir, root, kind, &mut out);
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

fn walk(dir: &Path, root: &Path, kind: Kind, out: &mut Vec<Fixture>) {
    let main = dir.join("main.ppl");
    let is_case_dir = main.is_file() && dir != root.join("run") && dir != root.join("fail");
    if is_case_dir {
        out.push(fixture(main, root, kind));
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
    paths.sort();
    for p in paths {
        if p.is_dir() {
            walk(&p, root, kind, out);
        } else if p.extension().is_some_and(|e| e == "ppl") {
            out.push(fixture(p, root, kind));
        }
    }
}

fn fixture(path: PathBuf, root: &Path, kind: Kind) -> Fixture {
    let name = path
        .strip_prefix(root)
        .unwrap_or(&path)
        .to_string_lossy()
        .replace('\\', "/");
    Fixture { path, name, kind }
}

#[derive(Debug)]
pub enum Outcome {
    Pass,
    /// Everything that could run passed, but some checks need stages that don't exist yet.
    Pending(String),
    Fail(String),
}

/// Runs one fixture through the stages that exist.
pub fn run(f: &Fixture) -> Outcome {
    let text = match std::fs::read_to_string(&f.path) {
        Ok(t) => t,
        Err(e) => return Outcome::Fail(format!("cannot read: {e}")),
    };
    let directives = match parse_directives(&text) {
        Ok(d) => d,
        Err(problems) => {
            return Outcome::Fail(format!("bad directives:\n  {}", problems.join("\n  ")));
        }
    };
    let dir = f.path.parent().unwrap_or(Path::new("."));
    let checked = ppl_driver::check_source_in(f.name.clone(), text, dir);
    match f.kind {
        Kind::Run => run_test(&directives, &checked),
        Kind::Fail => fail_test(&directives, &checked),
    }
}

fn run_test(d: &Directives, checked: &ppl_driver::Checked) -> Outcome {
    if !d.errors.is_empty() {
        return Outcome::Fail("`error` directives belong in tests/fail, not tests/run".into());
    }
    if let Some(missing) = only_unimplemented(checked) {
        return Outcome::Pending(missing);
    }
    if checked.has_errors() {
        return Outcome::Fail(format!(
            "expected the program to compile, but it has errors:\n\n{}",
            checked.render_diagnostics()
        ));
    }
    let mir = checked.mir.as_ref().expect("MIR after a clean check");
    let mut out: Vec<u8> = Vec::new();
    let result = ppl_driver::run_interp(mir, &mut out);
    let err = result.err();
    let uncaught = err.as_ref().is_some_and(|p| p.uncaught);
    let interp = Run {
        stdout: String::from_utf8_lossy(&out).into_owned(),
        exit: uncaught.then_some(ppl_driver::UNCAUGHT_EXIT_CODE),
        panic: err.filter(|p| !p.uncaught).map(|p| p.msg),
    };
    let mut problems: Vec<String> = compare_run(d, &interp, "interpreter").into_iter().collect();
    match run_native(checked) {
        Ok(native) => problems.extend(compare_run(d, &native, "native")),
        Err(e) => problems.push(format!("[native] {e}")),
    }
    if problems.is_empty() {
        Outcome::Pass
    } else {
        Outcome::Fail(problems.join("\n\n"))
    }
}

/// Builds the program (cached) and runs the executable.
fn run_native(checked: &ppl_driver::Checked) -> Result<Run, String> {
    let exe = ppl_driver::build_cached(checked, ppl_driver::BuildOptions::default())?;
    let out = std::process::Command::new(&exe)
        .output()
        .map_err(|e| format!("cannot run {}: {e}", exe.display()))?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    let code = out.status.code().unwrap_or(-1);
    let panic = (code == ppl_driver::PANIC_EXIT_CODE).then(|| {
        stderr
            .lines()
            .next()
            .and_then(|l| l.strip_prefix("panic: "))
            .unwrap_or("")
            .to_string()
    });
    Ok(Run {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        panic,
        exit: Some(code),
    })
}

/// What a program did.
pub struct Run {
    pub stdout: String,
    /// The panic message, if it panicked.
    pub panic: Option<String>,
    /// The process exit code (native runs); derived from `panic` for the interpreter.
    pub exit: Option<i32>,
}

impl Run {
    pub fn exit_code(&self) -> i32 {
        self.exit.unwrap_or(if self.panic.is_some() {
            ppl_driver::PANIC_EXIT_CODE
        } else {
            0
        })
    }
}

/// Checks a run against the directives; returns a readable report of the differences.
pub fn compare_run(d: &Directives, run: &Run, mode: &str) -> Option<String> {
    let mut problems = String::new();
    let mut actual: Vec<String> = run.stdout.split('\n').map(str::to_string).collect();
    if actual.last().is_some_and(String::is_empty) {
        actual.pop();
    }
    if actual != d.expect {
        let _ = write!(
            problems,
            "stdout differs (- expected, + actual):\n{}",
            diff_lines(&d.expect, &actual)
        );
    }
    let want_exit = d.exit.unwrap_or(if d.panic.is_some() {
        ppl_driver::PANIC_EXIT_CODE
    } else {
        0
    });
    if run.exit_code() != want_exit {
        let _ = writeln!(
            problems,
            "exit code: expected {want_exit}, got {}",
            run.exit_code()
        );
    }
    match (&d.panic, &run.panic) {
        (Some(want), Some(got)) if want != got => {
            let _ = writeln!(problems, "panic message: expected {want:?}, got {got:?}");
        }
        (Some(want), None) => {
            let _ = writeln!(
                problems,
                "expected a panic ({want:?}), but the program finished normally"
            );
        }
        (None, Some(got)) => {
            let _ = writeln!(problems, "unexpected panic: {got}");
        }
        _ => {}
    }
    (!problems.is_empty()).then(|| format!("[{mode}] {}", problems.trim_end()))
}

fn fail_test(d: &Directives, checked: &ppl_driver::Checked) -> Outcome {
    if d.errors.is_empty() {
        return Outcome::Fail(
            "a tests/fail fixture needs at least one `// error: E0xxx` directive".into(),
        );
    }
    if !d.expect.is_empty() || d.exit.is_some() || d.panic.is_some() {
        return Outcome::Fail("`expect`/`exit`/`panic` directives belong in tests/run".into());
    }
    let actual: Vec<(Code, (usize, usize))> = checked
        .diags
        .iter()
        .filter(|diag| diag.is_error())
        .map(|diag| {
            let pos = diag
                .main_span()
                .map_or((0, 0), |s| checked.sm.file(s.file).line_col(s.start));
            (diag.code.unwrap_or(Code(0)), pos)
        })
        .collect();

    // Expected errors from stages that don't exist yet can't be checked. The errors the
    // implemented stages report must still match the expected ones from those stages.
    let missing: Vec<Stage> = {
        let mut v: Vec<Stage> = d
            .errors
            .iter()
            .map(|e| Stage::of(e.code))
            .filter(|s| !s.is_implemented())
            .collect();
        v.sort();
        v.dedup();
        v
    };
    let checkable: Vec<&ExpectedError> = d
        .errors
        .iter()
        .filter(|e| Stage::of(e.code).is_implemented())
        .collect();

    let fmt_expected = |e: &ExpectedError| match e.at {
        Some((l, c)) => format!("{} at {l}:{c}", e.code),
        None => e.code.to_string(),
    };
    let expected_lines: Vec<String> = checkable.iter().map(|e| fmt_expected(e)).collect();
    // Show actual positions only where the expectation gives one, so diffs stay aligned.
    let actual_lines: Vec<String> = actual
        .iter()
        .enumerate()
        .map(
            |(i, (code, (l, c)))| match checkable.get(i).and_then(|e| e.at) {
                Some(_) => format!("{code} at {l}:{c}"),
                None => code.to_string(),
            },
        )
        .collect();

    if expected_lines != actual_lines
        && let Some(missing) = only_unimplemented_among(checked, &expected_lines)
    {
        return Outcome::Pending(missing);
    }
    if expected_lines != actual_lines {
        let mut msg = String::from("compile errors don't match (- expected, + actual):\n");
        msg.push_str(&diff_lines(&expected_lines, &actual_lines));
        if !missing.is_empty() {
            let _ = write!(
                msg,
                "\n(errors from the {} aren't checked yet)\n",
                stage_names(&missing)
            );
        }
        if !checked.diags.is_empty() {
            let _ = write!(
                msg,
                "\nactual diagnostics:\n\n{}",
                checked.render_diagnostics()
            );
        }
        return Outcome::Fail(msg);
    }
    if !missing.is_empty() {
        let codes: Vec<String> = d
            .errors
            .iter()
            .filter(|e| !Stage::of(e.code).is_implemented())
            .map(|e| e.code.to_string())
            .collect();
        return Outcome::Pending(format!(
            "{}: waiting for the {}",
            codes.join(", "),
            stage_names(&missing)
        ));
    }
    Outcome::Pass
}

/// If every error is "not implemented yet" (`E0900`), describes what's missing.
fn only_unimplemented(checked: &ppl_driver::Checked) -> Option<String> {
    let errors: Vec<_> = checked.diags.iter().filter(|d| d.is_error()).collect();
    if errors.is_empty()
        || !errors
            .iter()
            .all(|d| d.code == Some(codes::NOT_IMPLEMENTED))
    {
        return None;
    }
    Some(unimplemented_list(checked))
}

/// A fail test is pending if the compiler hit unimplemented features (`E0900`) that it wasn't
/// expected to report.
fn only_unimplemented_among(checked: &ppl_driver::Checked, expected: &[String]) -> Option<String> {
    let has_unimpl = checked
        .diags
        .iter()
        .any(|d| d.code == Some(codes::NOT_IMPLEMENTED));
    let expects_unimpl = expected.iter().any(|e| e.starts_with("E0900"));
    (has_unimpl && !expects_unimpl).then(|| unimplemented_list(checked))
}

fn unimplemented_list(checked: &ppl_driver::Checked) -> String {
    let mut what: Vec<String> = checked
        .diags
        .iter()
        .filter(|d| d.code == Some(codes::NOT_IMPLEMENTED))
        .map(|d| {
            d.message
                .trim_end_matches(" isn't implemented yet")
                .to_string()
        })
        .collect();
    what.sort();
    what.dedup();
    format!(
        "uses features that aren't implemented yet: {}",
        what.join(", ")
    )
}

fn stage_names(stages: &[Stage]) -> String {
    stages
        .iter()
        .map(|s| s.name())
        .collect::<Vec<_>>()
        .join(" and ")
}

/// A line diff (longest common subsequence): ` ` same, `-` only in `expected`, `+` only in
/// `actual`.
pub fn diff_lines(expected: &[String], actual: &[String]) -> String {
    let (n, m) = (expected.len(), actual.len());
    let mut lcs = vec![vec![0usize; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[i][j] = if expected[i] == actual[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }
    let mut out = String::new();
    let (mut i, mut j) = (0, 0);
    while i < n || j < m {
        if i < n && j < m && expected[i] == actual[j] {
            let _ = writeln!(out, "    {}", expected[i]);
            i += 1;
            j += 1;
        } else if j < m && (i == n || lcs[i][j + 1] >= lcs[i + 1][j]) {
            let _ = writeln!(out, "  + {}", actual[j]);
            j += 1;
        } else {
            let _ = writeln!(out, "  - {}", expected[i]);
            i += 1;
        }
    }
    out
}

/// Results of a whole run.
pub struct Report {
    pub passed: Vec<String>,
    pub pending: Vec<(String, String)>,
    pub failed: Vec<(String, String)>,
}

impl Report {
    pub fn summary(&self) -> String {
        format!(
            "golden tests: {} passed, {} pending, {} failed",
            self.passed.len(),
            self.pending.len(),
            self.failed.len()
        )
    }

    /// A readable report: failures in full, pending fixtures as one line each.
    pub fn render(&self) -> String {
        let mut out = String::new();
        for (name, msg) in &self.failed {
            let _ = writeln!(out, "──── FAIL {name}\n{}", msg.trim_end());
            out.push('\n');
        }
        if !self.pending.is_empty() {
            out.push_str("pending:\n");
            for (name, why) in &self.pending {
                let _ = writeln!(out, "  {name}: {why}");
            }
        }
        out.push_str(&self.summary());
        out.push('\n');
        out
    }
}

/// Runs every fixture under `root` whose name contains `filter` (if given).
pub fn run_all(root: &Path, filter: Option<&str>) -> Report {
    let mut report = Report {
        passed: Vec::new(),
        pending: Vec::new(),
        failed: Vec::new(),
    };
    for f in discover(root) {
        if filter.is_some_and(|flt| !f.name.contains(flt)) {
            continue;
        }
        match run(&f) {
            Outcome::Pass => report.passed.push(f.name),
            Outcome::Pending(why) => report.pending.push((f.name, why)),
            Outcome::Fail(msg) => report.failed.push((f.name, msg)),
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directives() {
        let d = parse_directives(
            "// expect: Hello\n// expect:\n// expect:   two  \n// exit: 3\n// panic: boom\nx\n",
        )
        .unwrap();
        assert_eq!(d.expect, vec!["Hello", "", "  two  "]);
        assert_eq!(d.exit, Some(3));
        assert_eq!(d.panic.as_deref(), Some("boom"));

        let d = parse_directives("// error: E0308\n// at: 4:19\n// error: E0100\n").unwrap();
        assert_eq!(d.errors.len(), 2);
        assert_eq!(d.errors[0].at, Some((4, 19)));
        assert_eq!(d.errors[1].at, None);

        // Ordinary comments that merely contain a colon are ignored.
        assert_eq!(
            parse_directives("// note: hi\n// see: x\n").unwrap(),
            Directives::default()
        );
    }

    #[test]
    fn malformed_directives_are_reported() {
        let errs =
            parse_directives("// error: 308\n// at: 3\n// exit: x\n// at: 1:1\n").unwrap_err();
        assert_eq!(errs.len(), 4, "{errs:?}");
    }

    #[test]
    fn diff_marks_changes() {
        let e: Vec<String> = ["a", "b", "c"].map(String::from).to_vec();
        let a: Vec<String> = ["a", "x", "c", "d"].map(String::from).to_vec();
        assert_eq!(diff_lines(&e, &a), "    a\n  + x\n  - b\n    c\n  + d\n");
    }
}
