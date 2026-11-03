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
    // Each with both backends: LLVM on linear memory, and WasmGC (P8.7f).
    let runs = entries
        .iter()
        .flat_map(|e| [(e.clone(), false), (e.clone(), true)]);
    for (src, gc) in runs {
        let name = src.file_stem().unwrap().to_string_lossy().into_owned();
        let label = if gc {
            format!("{name} (WasmGC)")
        } else {
            name.clone()
        };
        let out = std::env::temp_dir().join(format!(
            "ppl-browser-{}-{name}{}",
            std::process::id(),
            if gc { "-gc" } else { "" }
        ));
        let _ = std::fs::remove_dir_all(&out);
        std::fs::create_dir_all(&out).unwrap();
        let opts = ppl_driver::BuildOptions {
            target: Some(target),
            ..Default::default()
        };
        let checked = ppl_driver::check_file_with(&src, opts).unwrap();
        if checked.has_errors() {
            failed.push(format!("{label}: {}", checked.render_diagnostics()));
            continue;
        }
        let built = if gc {
            ppl_driver::build_wasmgc(&checked, &out.join(&name))
        } else {
            ppl_driver::build_native(&checked, &out.join(&name), opts)
        };
        if let Err(e) = built {
            failed.push(format!("{label}: {e}"));
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
                "{label}: exit {:?}\n--- got\n{got}--- want\n{want}--- stderr\n{}",
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

/// The WasmGC backend (P8.7a): a few golden fixtures it covers, run under Node.js.
#[test]
fn wasmgc() {
    let node = std::process::Command::new("node").arg("--version").output();
    if node.is_err() {
        eprintln!("note: no Node.js: skipping the WasmGC backend test");
        return;
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/run");
    let out = std::env::temp_dir().join(format!("ppl-wasmgc-{}", std::process::id()));
    std::fs::create_dir_all(&out).unwrap();
    for (name, want, code) in [
        ("fib", "55\n6765\n", 0),
        ("hello", "Hello, world!\n", 0),
        ("overflow_panic", "255\n", 101),
    ]
    .map(|(n, w, c)| (n, w.to_string(), c))
    .into_iter()
    // (P8.7b–d: arrays, strings, format specs, objects, async, errors; the fixtures' expectations.)
    .chain(
        [
            "arrays",
            "format_specs",
            "chars",
            "closures",
            "enums",
            "inheritance",
            "interfaces",
            "unions",
            "to_string",
            "async/basics",
            "async/timers",
            "errors/exceptions",
            "errors/throwables",
        ]
        .map(|n| {
            let text = std::fs::read_to_string(root.join(format!("{n}.ppl"))).unwrap();
            let want: String = text
                .lines()
                .filter_map(|l| {
                    l.strip_prefix("// expect: ")
                        .or_else(|| (l == "// expect:").then_some(""))
                })
                .map(|l| format!("{l}\n"))
                .collect();
            let code = text
                .lines()
                .find_map(|l| l.strip_prefix("// exit: ")?.trim().parse().ok())
                .unwrap_or(0);
            (n, want, code)
        }),
    ) {
        let checked = ppl_driver::check_file(&root.join(format!("{name}.ppl"))).unwrap();
        let flat = name.replace('/', "_");
        ppl_driver::build_wasmgc(&checked, &out.join(&flat)).unwrap();
        let run = std::process::Command::new("node")
            .arg(out.join(format!("{flat}.js")))
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&run.stdout), want, "{name}");
        assert_eq!(run.status.code(), Some(code), "{name}");
    }
    let _ = std::fs::remove_dir_all(&out);
}
