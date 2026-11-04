//! Packages (spec §8.3, PLAN P10.3a): imports by package name from workspace members, their
//! errors, `devDependencies` for tests, the manifest, `entry`.

use pico_driver::packages::{Manifest, entry_of};
use std::path::{Path, PathBuf};

fn write(p: PathBuf, text: &str) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

/// The errors of checking `file` (their notes appended), one string each.
fn errors(file: &Path) -> Vec<String> {
    let text = std::fs::read_to_string(file).unwrap();
    let c = pico_driver::check_source_with(
        file.display().to_string(),
        text,
        file.parent().unwrap(),
        pico_driver::BuildOptions::default(),
    );
    c.diags
        .iter()
        .filter(|d| d.is_error())
        .map(|d| format!("{} {}", d.message, d.notes.join(" ")))
        .collect()
}

fn workspace(name: &str) -> PathBuf {
    let ws = std::env::temp_dir().join(format!("pico-packages-{}-{name}", std::process::id()));
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
        ws.join("packages/greet/index.pi"),
        "/** Greets. */\nexport function greet(who: string): string { `hello ${who}` }\n",
    );
    write(
        ws.join("packages/empty/project.json"),
        r#"{ "name": "demo/empty", "version": "1.0.0" }"#,
    );
    write(ws.join("packages/empty/lib.pi"), "export const X = 1\n");
    write(
        ws.join("tools/lint/project.json"),
        r#"{ "name": "demo/lint", "version": "2.3.0" }"#,
    );
    write(
        ws.join("tools/lint/index.pi"),
        "/** Checks. */\nexport function check(): bool { true }\n",
    );
    write(
        ws.join("packages/app/project.json"),
        r#"{ "name": "demo/app", "version": "1.0.0", "entry": "src/main.pi",
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
        app.join("src/main.pi"),
        "import { greet } from \"demo/greet\"\nprintln(greet(\"ws\"))\n",
    );
    assert_eq!(errors(&app.join("src/main.pi")), Vec::<String>::new());
    // `entry` names the main file.
    assert_eq!(
        entry_of(&app.join("src")).unwrap(),
        app.canonicalize().unwrap().join("src/main.pi")
    );
}

#[test]
fn import_errors() {
    let ws = workspace("errors");
    let app = ws.join("packages/app");
    let check = |src: &str| {
        write(app.join("src/t.pi"), src);
        errors(&app.join("src/t.pi")).join("\n")
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
        ws.join("packages/lint2/index.pi"),
        "/** Two. */\nexport const TWO = 2\n",
    );
    let src = "import { TWO } from \"demo/lint2\"\nprintln(TWO)\n";
    write(app.join("src/app.test.pi"), src);
    assert_eq!(errors(&app.join("src/app.test.pi")), Vec::<String>::new());
    write(app.join("src/main.pi"), src);
    assert!(
        errors(&app.join("src/main.pi"))
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

// ---------------------------------------------------------------- git dependencies (P10.3b)

/// A git repository with a package `name` at `version`, its `index.pi` exporting `what()`,
/// tagged `v<version>`; more versions are commits and tags on top.
fn repo(dir: &Path, name: &str, versions: &[(&str, &str)]) {
    let git = |args: &[&str]| {
        let ok = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "git {args:?}");
    };
    std::fs::create_dir_all(dir).unwrap();
    git(&["init", "-q", "-b", "main"]);
    git(&["config", "user.email", "t@example.com"]);
    git(&["config", "user.name", "t"]);
    for (version, what) in versions {
        write(
            dir.join("project.json"),
            &format!(r#"{{ "name": "{name}", "version": "{version}" }}"#),
        );
        write(
            dir.join("index.pi"),
            &format!("/** What. */\nexport function what(): string {{ \"{what}\" }}\n"),
        );
        git(&["add", "-A"]);
        git(&["commit", "-qm", version]);
        git(&["tag", &format!("v{version}")]);
    }
}

fn have_git() -> bool {
    std::process::Command::new("git")
        .arg("--version")
        .output()
        .is_ok()
}

/// A workspace whose `demo/util` member uses `ext/colors` at `util_tag`, and whose `demo/app`
/// uses `demo/util` and, when given, `ext/colors` at `app_tag` too.
fn git_workspace(name: &str, util_tag: &str, app_tag: Option<&str>) -> (PathBuf, PathBuf) {
    let base = std::env::temp_dir().join(format!("pico-gitdeps-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    // (Each test its own cache: tests run in parallel.)
    let repo_dir = base.join("colors");
    repo(
        &repo_dir,
        "ext/colors",
        &[
            ("1.2.0", "one-two"),
            ("1.3.0", "one-three"),
            ("2.0.0", "two"),
        ],
    );
    let url = repo_dir.display().to_string();
    let ws = base.join("ws");
    write(
        ws.join("project.json"),
        r#"{ "workspace": ["packages/*"] }"#,
    );
    write(
        ws.join("packages/util/project.json"),
        &format!(
            r#"{{ "name": "demo/util", "version": "0.1.0", "dependencies": {{ "ext/colors": {{ "git": "{url}", "tag": "{util_tag}" }} }} }}"#
        ),
    );
    write(
        ws.join("packages/util/index.pi"),
        "import { what } from \"ext/colors\"\n/** Says. */\nexport function say(): string { what() }\n",
    );
    let app_dep = app_tag
        .map(|t| format!(r#", "ext/colors": {{ "git": "{url}", "tag": "{t}" }}"#))
        .unwrap_or_default();
    write(
        ws.join("packages/app/project.json"),
        &format!(
            r#"{{ "name": "demo/app", "version": "1.0.0", "entry": "src/main.pi", "dependencies": {{ "demo/util": "^0.1"{app_dep} }} }}"#
        ),
    );
    let main = if app_tag.is_some() {
        "import { say } from \"demo/util\"\nimport { what } from \"ext/colors\"\nprintln(`${say()} ${what()}`)\n"
    } else {
        "import { say } from \"demo/util\"\nprintln(say())\n"
    };
    write(ws.join("packages/app/src/main.pi"), main);
    (base, ws)
}

fn with_cache<T>(base: &Path, f: impl FnOnce() -> T) -> T {
    // (The cache directory comes from the environment; tests set it to their own.)
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    unsafe { std::env::set_var("PICO_CACHE_DIR", base.join("cache")) };
    f()
}

#[test]
fn git_dependency_builds_offline_from_the_lockfile() {
    if !have_git() {
        return;
    }
    let (base, ws) = git_workspace("offline", "v1.2.0", None);
    let app = ws.join("packages/app");
    with_cache(&base, || {
        // Locked (and fetched) once…
        let p = pico_driver::packages::prepare_with(&app, true)
            .unwrap()
            .unwrap();
        assert_eq!(p.lock.packages.len(), 3, "{:#?}", p.lock);
        let lock = std::fs::read_to_string(ws.join("project.lock")).unwrap();
        assert!(
            lock.contains("\"ref\": \"tag:v1.2.0\"") && lock.contains("\"hash\": \"sha256:"),
            "{lock}"
        );
        // …then built without the repository: from the lockfile and the cache.
        std::fs::rename(base.join("colors"), base.join("colors.gone")).unwrap();
        assert_eq!(errors(&app.join("src/main.pi")), Vec::<String>::new());
        // A changed cached source doesn't build.
        let checkout = pico_driver::packages::prepare_with(&app, false)
            .unwrap()
            .unwrap()
            .dirs["ext/colors@1.2.0"]
            .clone();
        let index = checkout.join("index.pi");
        let orig = std::fs::read_to_string(&index).unwrap();
        std::fs::write(&index, format!("{orig}// changed\n")).unwrap();
        assert!(
            errors(&app.join("src/main.pi"))
                .join("\n")
                .contains("doesn't match its hash")
        );
        std::fs::write(&index, orig).unwrap();
        // A dependency the lockfile doesn't have yet needs `pico fetch` (no fetching here).
        let util = ws.join("packages/util/project.json");
        let text = std::fs::read_to_string(&util).unwrap();
        std::fs::write(&util, text.replace("v1.2.0", "v1.3.0")).unwrap();
        assert!(
            errors(&app.join("src/main.pi"))
                .join("\n")
                .contains("run `pico fetch`")
        );
    });
}

#[test]
fn majors_unify_or_coexist() {
    if !have_git() {
        return;
    }
    // One major: two different commits are a conflict.
    let (base, ws) = git_workspace("one-major", "v1.2.0", Some("v1.3.0"));
    with_cache(&base, || {
        let e = pico_driver::packages::prepare_with(&ws.join("packages/app"), true).unwrap_err();
        assert!(e.contains("versions of one major must be one"), "{e}");
    });
    // Different majors coexist: each dependent gets its own.
    let (base, ws) = git_workspace("two-majors", "v1.2.0", Some("v2.0.0"));
    let app = ws.join("packages/app");
    with_cache(&base, || {
        let p = pico_driver::packages::prepare_with(&app, true)
            .unwrap()
            .unwrap();
        assert!(
            p.lock.packages.contains_key("ext/colors@1.2.0")
                && p.lock.packages.contains_key("ext/colors@2.0.0")
        );
        assert_eq!(errors(&app.join("src/main.pi")), Vec::<String>::new());
    });
}
