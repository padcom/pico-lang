//! The standard library's usage examples (decision 348): every function, method, constructor,
//! property and type of the standard and core libraries has one, every `@example` in their doc
//! comments is a program that type-checks, and `website/reference.json` (the website's reference
//! page) is what `pico doc --std --json` makes of them.
//!
//! `std/js` examples are checked for `wasm32-browser`, the only target that has it. An example
//! that needs another module (a worker's) has it after a line `// --- job.pi ---`: the files
//! are written next to each other, the program first.
//!
//! - `PICO_DOC_EXAMPLES=std/path,core` checks only those pages' examples.
//! - `PICO_DOC_EXAMPLES_RUN=1` also runs each example (but `std/js`'s, in the interpreter) and
//!   prints what it printed, to compare with the `// …` comments that say what it prints.
//! - `PICO_DOC_EXAMPLES_JOBS=2` checks two at a time (default: the CPUs, at most 8).
//! - `PICO_UPDATE_REFERENCE=1` writes `website/reference.json` instead of comparing it.
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;

fn temp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pico-doc-examples-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// `pico doc --std --json`, read back.
fn reference(dir: &Path) -> (String, Value) {
    let file = dir.join("reference.json");
    let out = Command::new(env!("CARGO_BIN_EXE_pico"))
        .args(["doc", "--std", "--json"])
        .arg(&file)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = std::fs::read_to_string(&file).unwrap();
    let json = serde_json::from_str(&text).unwrap();
    (text, json)
}

/// One example: where it is (`std/path › join`) and its code.
struct Sample {
    label: String,
    code: String,
    /// Checked for the browser (`std/js` exists only there), and not run.
    browser: bool,
}

fn samples(reference: &Value) -> Vec<Sample> {
    fn walk(out: &mut Vec<Sample>, at: &str, browser: bool, items: &Value) {
        for i in items.as_array().into_iter().flatten() {
            let label = format!("{at} › {}", i["name"].as_str().unwrap_or("?"));
            for (n, e) in i["examples"].as_array().into_iter().flatten().enumerate() {
                out.push(Sample {
                    label: format!("{label} (example {})", n + 1),
                    code: e["code"].as_str().unwrap_or("").to_string(),
                    browser,
                });
            }
            walk(out, &label, browser, &i["members"]);
        }
    }
    let only: Option<Vec<String>> = std::env::var("PICO_DOC_EXAMPLES")
        .ok()
        .map(|v| v.split(',').map(|s| s.trim().to_string()).collect());
    let mut out = Vec::new();
    for m in reference["modules"].as_array().unwrap() {
        let name = m["name"].as_str().unwrap();
        if only.as_ref().is_none_or(|o| o.iter().any(|x| x == name)) {
            walk(&mut out, name, name == "std/js", &m["items"]);
        }
    }
    out
}

/// Writes an example's files into a directory of their own: the program (`main.pi`, returned
/// with its text) and the modules after `// --- name.pi ---` lines.
fn write_files(dir: &Path, n: usize, code: &str) -> (PathBuf, String) {
    let dir = dir.join(format!("example{n}"));
    std::fs::create_dir_all(&dir).unwrap();
    let mut main = String::new();
    let mut current: Option<(String, String)> = None;
    for line in code.lines() {
        let marker = line
            .strip_prefix("// --- ")
            .and_then(|l| l.strip_suffix(" ---"))
            .filter(|f| f.ends_with(".pi"));
        if let Some(f) = marker {
            if let Some((name, text)) = current.take() {
                std::fs::write(dir.join(name), text).unwrap();
            }
            current = Some((f.to_string(), String::new()));
            continue;
        }
        let out = current.as_mut().map_or(&mut main, |(_, t)| t);
        out.push_str(line);
        out.push('\n');
    }
    if let Some((name, text)) = current {
        std::fs::write(dir.join(name), text).unwrap();
    }
    let file = dir.join("main.pi");
    std::fs::write(&file, &main).unwrap();
    (file, main)
}

/// The entries that need an example and have none: functions, methods, constructors,
/// properties and types (fields, constants and type aliases are shown by their type's or
/// module's examples).
fn without_examples(reference: &Value) -> Vec<String> {
    fn walk(out: &mut Vec<String>, at: &str, items: &Value) {
        for i in items.as_array().into_iter().flatten() {
            let label = format!("{at} › {}", i["name"].as_str().unwrap_or("?"));
            let needs = matches!(
                i["kind"].as_str(),
                Some(
                    "function"
                        | "method"
                        | "constructor"
                        | "property"
                        | "class"
                        | "struct"
                        | "interface"
                        | "enum"
                )
            );
            if needs && i["examples"].as_array().is_none_or(Vec::is_empty) {
                let at = i["source"].as_str().unwrap_or("the compiler");
                out.push(format!("{label} ({at})"));
            }
            walk(out, &label, &i["members"]);
        }
    }
    let mut out = Vec::new();
    for m in reference["modules"].as_array().unwrap() {
        walk(&mut out, m["name"].as_str().unwrap(), &m["items"]);
    }
    out
}

/// The errors of checking an example (for the browser, if `browser`).
fn check(dir: &Path, n: usize, code: &str, browser: bool) -> Vec<String> {
    let (file, code) = write_files(dir, n, code);
    let dir = file.parent().unwrap();
    let name = file.display().to_string();
    let opts = pico_driver::BuildOptions {
        target: browser.then(|| pico_driver::Target::parse("wasm32-browser").unwrap()),
        ..pico_driver::BuildOptions::default()
    };
    let checked =
        std::panic::catch_unwind(|| pico_driver::check_source_with(name, &code, dir, opts));
    match checked {
        Ok(c) => c
            .diags
            .iter()
            .filter(|d| d.is_error())
            .map(|d| {
                let code = d.code.map(|c| format!("[{c:?}] ")).unwrap_or_default();
                format!("{code}{}", d.message)
            })
            .collect(),
        Err(_) => vec!["the compiler panicked".into()],
    }
}

#[test]
fn examples_type_check_and_the_website_reference_is_current() {
    let dir = temp("all");
    let (text, json) = reference(&dir);
    let website = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../website/reference.json");
    if std::env::var_os("PICO_UPDATE_REFERENCE").is_some() {
        std::fs::write(&website, &text).unwrap();
    } else if std::env::var_os("PICO_DOC_EXAMPLES").is_none() {
        let current = std::fs::read_to_string(&website).unwrap_or_default();
        assert!(
            current == text,
            "website/reference.json is out of date: run `PICO_UPDATE_REFERENCE=1 cargo test -p pico-cli --test doc_examples`"
        );
    }
    if std::env::var_os("PICO_DOC_EXAMPLES").is_none() {
        let missing = without_examples(&json);
        assert!(
            missing.is_empty(),
            "{} entries have no @example (docs/DEVELOPMENT.md, \"Standard library examples\"):\n  {}",
            missing.len(),
            missing.join("\n  ")
        );
    }
    let samples = samples(&json);
    let jobs = std::env::var("PICO_DOC_EXAMPLES_JOBS")
        .ok()
        .and_then(|j| j.parse::<usize>().ok())
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map_or(4, |n| n.get())
                .min(8)
        })
        .max(1);
    let failures: Vec<String> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..jobs)
            .map(|j| {
                let samples = &samples;
                let dir = &dir;
                s.spawn(move || {
                    let mut failed = Vec::new();
                    for (n, sample) in samples.iter().enumerate().skip(j).step_by(jobs) {
                        let errors = check(dir, n, &sample.code, sample.browser);
                        if !errors.is_empty() {
                            failed.push(format!(
                                "{}:\n  {}\n--- code ---\n{}\n",
                                sample.label,
                                errors.join("\n  "),
                                sample.code
                            ));
                        }
                    }
                    failed
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().unwrap())
            .collect()
    });
    if std::env::var_os("PICO_DOC_EXAMPLES_RUN").is_some() {
        for (n, sample) in samples.iter().enumerate().filter(|(_, s)| !s.browser) {
            let (file, _) = write_files(&dir.join("run"), n, &sample.code);
            let out = Command::new(env!("CARGO_BIN_EXE_pico"))
                .args(["run", "--interp"])
                .arg(&file)
                .current_dir(file.parent().unwrap())
                .output()
                .unwrap();
            println!(
                "=== {} (exit {})\n{}{}",
                sample.label,
                out.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }
    println!("{} examples checked", samples.len());
    assert!(
        failures.is_empty(),
        "{} of {} examples don't type-check:\n\n{}",
        failures.len(),
        samples.len(),
        failures.join("\n")
    );
}
