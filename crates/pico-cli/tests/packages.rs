//! The package commands (spec §8.3, PLAN P10.3c) on a scratch project: `pico init`, `pico add`
//! (a git repository), `pico run` with it, `pico remove`; the manifest and the lockfile after each.

use std::path::Path;
use std::process::Command;

fn pico(args: &[&str], dir: &Path, cache: &Path) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_pico"))
        .args(args)
        .current_dir(dir)
        .env("PICO_CACHE_DIR", cache)
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), text)
}

fn git(args: &[&str], dir: &Path) {
    let ok = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap()
        .status
        .success();
    assert!(ok, "git {args:?}");
}

#[test]
fn init_add_run_remove() {
    if Command::new("git").arg("--version").output().is_err() {
        return;
    }
    let base = std::env::temp_dir().join(format!("pico-commands-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let (repo, app, cache) = (base.join("colors"), base.join("app"), base.join("cache"));
    // A library in git with versions 1.0.0, 1.1.0 and a 2.0.0 pre-release.
    std::fs::create_dir_all(&repo).unwrap();
    git(&["init", "-q", "-b", "main"], &repo);
    git(&["config", "user.email", "t@example.com"], &repo);
    git(&["config", "user.name", "t"], &repo);
    for (v, word) in [
        ("1.0.0", "one"),
        ("1.1.0", "eleven"),
        ("2.0.0-beta.1", "beta"),
    ] {
        std::fs::write(
            repo.join("project.json"),
            format!(r#"{{ "name": "ext/colors", "version": "{v}" }}"#),
        )
        .unwrap();
        std::fs::write(
            repo.join("index.pi"),
            format!("/** A word. */\nexport function word(): string {{ \"{word}\" }}\n"),
        )
        .unwrap();
        git(&["add", "-A"], &repo);
        git(&["commit", "-qm", v], &repo);
        git(&["tag", &format!("v{v}")], &repo);
    }

    // init: an application with `entry`.
    std::fs::create_dir_all(&app).unwrap();
    let (ok, out) = pico(&["init", "demo/app"], &app, &cache);
    assert!(ok, "{out}");
    let manifest = std::fs::read_to_string(app.join("project.json")).unwrap();
    assert!(
        manifest.contains("\"entry\": \"src/main.pi\""),
        "{manifest}"
    );
    assert!(app.join("src/main.pi").is_file());
    let (ok, out) = pico(&["init"], &app, &cache);
    assert!(!ok && out.contains("exists already"), "{out}");
    let (ok, out) = pico(&["init", "--lib"], &base, &cache);
    assert!(!ok && out.contains("a library needs a name"), "{out}");

    // add: the newest release tag (not the pre-release), under the package's own name.
    let (ok, out) = pico(&["add", &repo.display().to_string()], &app, &cache);
    assert!(ok, "{out}");
    assert!(
        out.contains("added ext/colors") && out.contains("tag `v1.1.0`"),
        "{out}"
    );
    let manifest = std::fs::read_to_string(app.join("project.json")).unwrap();
    assert!(
        manifest.contains("\"ext/colors\"") && manifest.contains("\"tag\": \"v1.1.0\""),
        "{manifest}"
    );
    // (The manifest keeps its keys' order: `name` first, `dependencies` last.)
    assert!(
        manifest.find("\"name\"").unwrap() < manifest.find("\"dependencies\"").unwrap(),
        "{manifest}"
    );
    let lock = std::fs::read_to_string(app.join("project.lock")).unwrap();
    assert!(
        lock.contains("ext/colors@1.1.0") && lock.contains("sha256:"),
        "{lock}"
    );

    // run: the dependency works.
    std::fs::write(
        app.join("src/main.pi"),
        "import { word } from \"ext/colors\"\nprintln(word())\n",
    )
    .unwrap();
    let (ok, out) = pico(&["run"], &app, &cache);
    assert!(ok && out.contains("eleven"), "{out}");

    // remove: gone from the manifest and the lockfile.
    let (ok, out) = pico(&["remove", "ext/colors"], &app, &cache);
    assert!(ok, "{out}");
    assert!(
        !std::fs::read_to_string(app.join("project.json"))
            .unwrap()
            .contains("ext/colors")
    );
    assert!(
        !std::fs::read_to_string(app.join("project.lock"))
            .unwrap()
            .contains("ext/colors")
    );
    let (ok, out) = pico(&["remove", "ext/colors"], &app, &cache);
    assert!(!ok && out.contains("isn't a dependency"), "{out}");

    // A registry name: later (P10.4).
    let (ok, out) = pico(&["add", "ext/colors"], &app, &cache);
    assert!(!ok && out.contains("P10.4"), "{out}");
}

/// Relative paths inside a workspace member (#93): `pico build main.pi` and a module it imports
/// relatively (whose directory is then empty) still find the workspace, and no lockfile appears
/// in the member.
#[test]
fn relative_paths_in_a_workspace_member() {
    let ws = std::env::temp_dir().join(format!("pico-cli-relative-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&ws);
    let write = |path: &str, text: &str| {
        let p = ws.join(path);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    };
    write("project.json", r#"{ "workspace": ["lib", "apps/app"] }"#);
    write(
        "lib/project.json",
        r#"{ "name": "me/lib", "version": "0.1.0" }"#,
    );
    write("lib/index.pi", "/** One. */\nexport const ONE: int = 1\n");
    write(
        "apps/app/project.json",
        r#"{ "name": "me/app", "version": "0.1.0", "entry": "main.pi", "dependencies": { "me/lib": "0.1" } }"#,
    );
    write(
        "apps/app/main.pi",
        "import { two } from \"./two\"\nprintln(`${two()}`)\n",
    );
    write(
        "apps/app/two.pi",
        "import { ONE } from \"me/lib\"\nexport function two(): int { ONE + ONE }\n",
    );
    let app = ws.join("apps/app");
    let cache = ws.join("cache");
    for file in ["main.pi", "./main.pi"] {
        let (ok, out) = pico(&["run", file], &app, &cache);
        assert!(ok && out.contains('2'), "pico run {file}:\n{out}");
    }
    assert!(
        !app.join("project.lock").exists(),
        "a lockfile appeared in the member"
    );
    let _ = std::fs::remove_dir_all(&ws);
}
