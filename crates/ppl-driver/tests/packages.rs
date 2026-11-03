//! Packages (spec §8.3, PLAN P10.3a): imports by package name from workspace members, their
//! errors, `devDependencies` for tests, the manifest, `entry`.

use ppl_driver::packages::{Manifest, entry_of};
use std::path::{Path, PathBuf};

fn write(p: PathBuf, text: &str) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

/// The errors of checking `file` (their notes appended), one string each.
fn errors(file: &Path) -> Vec<String> {
    let text = std::fs::read_to_string(file).unwrap();
    let c = ppl_driver::check_source_with(
        file.display().to_string(),
        text,
        file.parent().unwrap(),
        ppl_driver::BuildOptions::default(),
    );
    c.diags
        .iter()
        .filter(|d| d.is_error())
        .map(|d| format!("{} {}", d.message, d.notes.join(" ")))
        .collect()
}

fn workspace(name: &str) -> PathBuf {
    let ws = std::env::temp_dir().join(format!("ppl-packages-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&ws);
    write(
        ws.join("project.json"),
        r#"{ "workspace": ["packages/*", "tools/lint"] }"#,
    );
    write(
        ws.join("packages/greet/project.json"),
        r#"{ "name": "demo/greet", "version": "0.2.1" }"#,
    );
    write(
        ws.join("packages/greet/index.ppl"),
        "/** Greets. */\nexport function greet(who: string): string { `hello ${who}` }\n",
    );
    write(
        ws.join("packages/empty/project.json"),
        r#"{ "name": "demo/empty", "version": "1.0.0" }"#,
    );
    write(ws.join("packages/empty/lib.ppl"), "export const X = 1\n");
    write(
        ws.join("tools/lint/project.json"),
        r#"{ "name": "demo/lint", "version": "2.3.0" }"#,
    );
    write(
        ws.join("tools/lint/index.ppl"),
        "/** Checks. */\nexport function check(): bool { true }\n",
    );
    write(
        ws.join("packages/app/project.json"),
        r#"{ "name": "demo/app", "version": "1.0.0", "entry": "src/main.ppl",
             "dependencies": { "demo/greet": "^0.2", "demo/empty": "1", "demo/lint": "^1" },
             "devDependencies": { "demo/lint2": "*" } }"#,
    );
    ws
}

#[test]
fn imports_from_workspace_members() {
    let ws = workspace("ok");
    let app = ws.join("packages/app");
    write(
        app.join("src/main.ppl"),
        "import { greet } from \"demo/greet\"\nprintln(greet(\"ws\"))\n",
    );
    assert_eq!(errors(&app.join("src/main.ppl")), Vec::<String>::new());
    // `entry` names the main file.
    assert_eq!(
        entry_of(&app.join("src")).unwrap(),
        app.canonicalize().unwrap().join("src/main.ppl")
    );
}

#[test]
fn import_errors() {
    let ws = workspace("errors");
    let app = ws.join("packages/app");
    let check = |src: &str| {
        write(app.join("src/t.ppl"), src);
        errors(&app.join("src/t.ppl")).join("\n")
    };
    assert!(
        check("import { x } from \"demo/greet/util\"\n")
            .contains("only a package's root: `demo/greet`")
    );
    assert!(check("import { X } from \"demo/empty\"\n").contains("exports nothing"));
    assert!(
        check("import { check } from \"demo/lint\"\n")
            .contains("`^1` isn't satisfied: the workspace has 2.3.0")
    );
    assert!(check("import { y } from \"demo/nope\"\n").contains("isn't a dependency of demo/app"));
    assert!(check("import { y } from \"greet\"\n").contains("isn't a module"));
}

#[test]
fn dev_dependencies_are_for_tests() {
    let ws = workspace("dev");
    let app = ws.join("packages/app");
    // (`demo/lint2` is a dev dependency; make it a member that satisfies `*`.)
    write(
        ws.join("packages/lint2/project.json"),
        r#"{ "name": "demo/lint2", "version": "0.1.0" }"#,
    );
    write(
        ws.join("packages/lint2/index.ppl"),
        "/** Two. */\nexport const TWO = 2\n",
    );
    let src = "import { TWO } from \"demo/lint2\"\nprintln(TWO)\n";
    write(app.join("src/app.test.ppl"), src);
    assert_eq!(errors(&app.join("src/app.test.ppl")), Vec::<String>::new());
    write(app.join("src/main.ppl"), src);
    assert!(
        errors(&app.join("src/main.ppl"))
            .join("\n")
            .contains("it's a devDependency")
    );
}

#[test]
fn manifest_validation() {
    let bad = Manifest::parse(
        Path::new("."),
        r#"{ "name": "NoScope", "version": "1.2", "dependencies": { "a/b": { "git": "x", "tag": "t", "rev": "r" }, "c/d": "^x" } }"#,
    )
    .unwrap_err()
    .join("\n");
    assert!(
        bad.contains("`name`: `NoScope` isn't `scope/name`"),
        "{bad}"
    );
    assert!(bad.contains("`version`: `1.2` isn't a version"), "{bad}");
    assert!(
        bad.contains("`dependencies.a/b`: give one of `tag`, `branch` and `rev`"),
        "{bad}"
    );
    assert!(bad.contains("`dependencies.c/d`"), "{bad}");
    let ok = Manifest::parse(
        Path::new("."),
        r#"{ "name": "demo/x", "version": "1.0.0-rc.1", "dependencies": { "a/b": { "git": "https://example.com/b", "branch": "main" } } }"#,
    );
    assert!(ok.is_ok());
}
