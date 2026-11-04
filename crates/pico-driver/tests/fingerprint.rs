//! Build fingerprints (spec §14.6, PLAN P10.5a): what changes makes a build build again.
//! (A placeholder file stands in for the output: fingerprints don't look inside it.)

use pico_driver::{BuildOptions, fingerprint};
use std::path::{Path, PathBuf};

fn check(entry: &Path) -> pico_driver::Checked {
    let text = std::fs::read_to_string(entry).unwrap();
    let c = pico_driver::check_source_with(
        entry.display().to_string(),
        text,
        entry.parent().unwrap(),
        BuildOptions::default(),
    );
    assert!(
        !c.diags.iter().any(|d| d.is_error()),
        "{}",
        c.render_diagnostics()
    );
    c
}

/// "Builds" `entry` into `exe` (writes it, records the fingerprint).
fn build(entry: &Path, exe: &Path, opts: &BuildOptions) {
    let c = check(entry);
    std::fs::write(exe, format!("built {:?}", std::time::SystemTime::now())).unwrap();
    fingerprint::record(&c, entry, exe, opts);
}

fn write(p: PathBuf, text: &str) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

#[test]
fn what_rebuilds() {
    let dir = std::env::temp_dir().join(format!("pico-fingerprint-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    // (The only test in this binary that uses the cache directory.)
    unsafe { std::env::set_var("PICO_CACHE_DIR", dir.join("cache")) };
    write(dir.join("p/project.json"), "{}");
    write(dir.join("p/data.txt"), "hello");
    write(
        dir.join("p/util.pi"),
        "export function twice(n: i64): i64 { n * 2 }\n",
    );
    let entry = dir.join("p/main.pi");
    write(
        entry.clone(),
        "import { twice } from \"./util\"\nconst DATA = comptime comptime.readFile(\"data.txt\")\nprintln(`${twice(2)} ${DATA}`)\n",
    );
    let exe = dir.join("p/out/app");
    std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
    let opts = BuildOptions::default();
    let fresh = || fingerprint::up_to_date(&entry, &exe, &opts);

    assert!(!fresh(), "never built");
    build(&entry, &exe, &opts);
    assert!(fresh(), "nothing changed");

    // A changed module.
    write(
        dir.join("p/util.pi"),
        "export function twice(n: i64): i64 { n + n }\n",
    );
    assert!(!fresh(), "an imported module changed");
    build(&entry, &exe, &opts);
    assert!(fresh());

    // A file comptime code read.
    write(dir.join("p/data.txt"), "bye");
    assert!(!fresh(), "comptime.readFile's file changed");
    build(&entry, &exe, &opts);

    // Other options, another output, a changed output.
    let release = BuildOptions {
        release: true,
        ..BuildOptions::default()
    };
    assert!(
        !fingerprint::up_to_date(&entry, &exe, &release),
        "other options"
    );
    assert!(
        !fingerprint::up_to_date(&entry, &dir.join("p/out/other"), &opts),
        "another output"
    );
    std::fs::write(
        &exe,
        "replaced, longer than before................................",
    )
    .unwrap();
    assert!(!fresh(), "the output was replaced");
    build(&entry, &exe, &opts);

    // project.json: changed, and a new one closer to the entry.
    write(dir.join("p/project.json"), "{ \"lint\": {} }");
    assert!(!fresh(), "project.json changed");
    build(&entry, &exe, &opts);
    assert!(fresh());
    std::fs::remove_file(&exe).unwrap();
    assert!(!fresh(), "the output is gone");
    build(&entry, &exe, &opts);
    fingerprint::forget(&exe);
    assert!(!fresh(), "forgotten");

    // Programs importing C headers aren't (system headers and pkg-config are outside the
    // compiler's view).
    let c = dir.join("p/c.pi");
    write(
        c.clone(),
        "import * as m from \"c:math.h\"\nprintln(m.fabs(-2.0))\n",
    );
    build(&c, &exe, &opts);
    assert!(!fingerprint::up_to_date(&c, &exe, &opts), "a C import");

    // Browser builds aren't fingerprinted.
    let browser = BuildOptions {
        target: Some(pico_driver::Target::parse("wasm32-browser").unwrap()),
        ..BuildOptions::default()
    };
    build(&entry, &exe, &browser);
    assert!(!fingerprint::up_to_date(&entry, &exe, &browser));
}
