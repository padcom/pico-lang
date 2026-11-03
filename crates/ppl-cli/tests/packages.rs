//! The package commands (spec §8.3, PLAN P10.3c) on a scratch project: `ppl init`, `ppl add`
//! (a git repository), `ppl run` with it, `ppl remove`; the manifest and the lockfile after each.

use std::path::Path;
use std::process::Command;

fn ppl(args: &[&str], dir: &Path, cache: &Path) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_ppl"))
        .args(args)
        .current_dir(dir)
        .env("PPL_CACHE_DIR", cache)
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
    let base = std::env::temp_dir().join(format!("ppl-commands-{}", std::process::id()));
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
            repo.join("index.ppl"),
            format!("/** A word. */\nexport function word(): string {{ \"{word}\" }}\n"),
        )
        .unwrap();
        git(&["add", "-A"], &repo);
        git(&["commit", "-qm", v], &repo);
        git(&["tag", &format!("v{v}")], &repo);
    }

    // init: an application with `entry`.
    std::fs::create_dir_all(&app).unwrap();
    let (ok, out) = ppl(&["init", "demo/app"], &app, &cache);
    assert!(ok, "{out}");
    let manifest = std::fs::read_to_string(app.join("project.json")).unwrap();
    assert!(
        manifest.contains("\"entry\": \"src/main.ppl\""),
        "{manifest}"
    );
    assert!(app.join("src/main.ppl").is_file());
    let (ok, out) = ppl(&["init"], &app, &cache);
    assert!(!ok && out.contains("exists already"), "{out}");
    let (ok, out) = ppl(&["init", "--lib"], &base, &cache);
    assert!(!ok && out.contains("a library needs a name"), "{out}");

    // add: the newest release tag (not the pre-release), under the package's own name.
    let (ok, out) = ppl(&["add", &repo.display().to_string()], &app, &cache);
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
        app.join("src/main.ppl"),
        "import { word } from \"ext/colors\"\nprintln(word())\n",
    )
    .unwrap();
    let (ok, out) = ppl(&["run"], &app, &cache);
    assert!(ok && out.contains("eleven"), "{out}");

    // remove: gone from the manifest and the lockfile.
    let (ok, out) = ppl(&["remove", "ext/colors"], &app, &cache);
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
    let (ok, out) = ppl(&["remove", "ext/colors"], &app, &cache);
    assert!(!ok && out.contains("isn't a dependency"), "{out}");

    // A registry name: later (P10.4).
    let (ok, out) = ppl(&["add", "ext/colors"], &app, &cache);
    assert!(!ok && out.contains("P10.4"), "{out}");
}
