//! wasm32-browser glue tests (decision 258): each `tests/browser/<name>.ppl` is built for the
//! browser target, then `<name>.mjs` drives it through the generated glue under Node.js and
//! its output must be `<name>.out`. Skipped (with a note) where `node` isn't installed.

use std::path::Path;

#[test]
fn browser() {
    // Wasm exception handling (exnref) needs Node.js 24 or newer.
    let node = std::process::Command::new("node").arg("--version").output();
    let major = node
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|v| {
            v.trim()
                .trim_start_matches('v')
                .split('.')
                .next()?
                .parse::<u32>()
                .ok()
        });
    if major.is_none_or(|m| m < 24) || !ppl_driver::LLD_AVAILABLE {
        eprintln!("note: no Node.js 24+ or no lld: skipping the wasm32-browser glue tests");
        return;
    }
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/browser");
    let target = ppl_driver::Target::parse("wasm32-browser").unwrap();
    let mut failed = Vec::new();
    let mut entries: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "ppl"))
        .collect();
    entries.sort();
    assert!(!entries.is_empty(), "no tests in {}", dir.display());
    for src in entries {
        let name = src.file_stem().unwrap().to_string_lossy().into_owned();
        let out = std::env::temp_dir().join(format!("ppl-browser-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&out);
        std::fs::create_dir_all(&out).unwrap();
        let opts = ppl_driver::BuildOptions {
            target: Some(target),
            ..Default::default()
        };
        let checked = ppl_driver::check_file_with(&src, opts).unwrap();
        if checked.has_errors() {
            failed.push(format!("{name}: {}", checked.render_diagnostics()));
            continue;
        }
        if let Err(e) = ppl_driver::build_native(&checked, &out.join(&name), opts) {
            failed.push(format!("{name}: {e}"));
            continue;
        }
        // Packages `js:npm/…` imports (the glue imports them from next to itself).
        copy_dir(&dir.join("node_modules"), &out.join("node_modules"));
        let driver = out.join(format!("{name}.mjs"));
        std::fs::copy(dir.join(format!("{name}.mjs")), &driver).unwrap();
        let run = std::process::Command::new("node")
            .arg(&driver)
            .output()
            .unwrap();
        let got = String::from_utf8_lossy(&run.stdout);
        let want = std::fs::read_to_string(dir.join(format!("{name}.out"))).unwrap();
        if !run.status.success() || got != want {
            failed.push(format!(
                "{name}: exit {:?}\n--- got\n{got}--- want\n{want}--- stderr\n{}",
                run.status.code(),
                String::from_utf8_lossy(&run.stderr)
            ));
        }
        let _ = std::fs::remove_dir_all(&out);
    }
    assert!(failed.is_empty(), "\n{}", failed.join("\n\n"));
}

fn copy_dir(from: &Path, to: &Path) {
    let Ok(entries) = std::fs::read_dir(from) else {
        return;
    };
    std::fs::create_dir_all(to).unwrap();
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            copy_dir(&p, &to.join(e.file_name()));
        } else {
            std::fs::copy(&p, to.join(e.file_name())).unwrap();
        }
    }
}
