//! Debug info (spec §14.5, PLAN P10.8a): lldb stops at a breakpoint set by a ppl file and line,
//! and the backtrace names the ppl functions with their files and lines (the core library's
//! too). Skipped where there's no lldb.

use ppl_driver::BuildOptions;
use std::path::PathBuf;
use std::process::Command;

fn lldb() -> Option<&'static str> {
    ["lldb", "lldb-23"].into_iter().find(|l| {
        Command::new(l)
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    })
}

#[test]
fn breakpoints_and_backtraces() {
    let Some(lldb) = lldb() else {
        eprintln!("note: no lldb: skipping");
        return;
    };
    let dir = std::env::temp_dir().join(format!("ppl-debug-info-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("util.ppl"),
        "export function twice(n: i64): i64 {\n  const r = n * 2\n  r\n}\n",
    )
    .unwrap();
    let main = "import { twice } from \"./util\"\n\nfunction show(x: i64) {\n  println(`${twice(x)} ${[1, 2].map((n) => twice(n)).length}`)\n}\nshow(21)\n";
    let entry = dir.join("main.ppl");
    std::fs::write(&entry, main).unwrap();
    let checked = ppl_driver::check_source_with(
        entry.display().to_string(),
        main.to_string(),
        &dir,
        BuildOptions::default(),
    );
    assert!(
        !checked.diags.iter().any(|d| d.is_error()),
        "{}",
        checked.render_diagnostics()
    );
    let exe: PathBuf = dir.join("app");
    ppl_driver::build_native(&checked, &exe, BuildOptions::default()).unwrap();
    let out = Command::new(lldb)
        .args(["-b", "-o", "breakpoint set -f util.ppl -l 2", "-o", "run"])
        .args(["-o", "bt", "-o", "continue", "-o", "bt", "-o", "kill"])
        .arg(&exe)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    // (Frames read `app`twice(n=21) at util.ppl:2`.)
    let frame = |f: &str, at: &str| {
        text.lines()
            .any(|l| l.contains(&format!("`{f}")) && l.contains(at))
    };
    // The breakpoint resolved to the ppl source line, after the parameters are in place.
    assert!(frame("twice", "at util.ppl:2"), "{text}");
    assert!(text.contains("twice(n=21)"), "{text}");
    // The first stop: called from `show` in main.ppl, line 4, from the top-level code.
    assert!(frame("show", "at main.ppl:4"), "{text}");
    assert!(frame("<main>", "at main.ppl:6"), "{text}");
    // The second: from the closure, through the core library's `map`.
    assert!(text.contains("array.ppl:"), "{text}");
}

#[test]
fn variables() {
    let Some(lldb) = lldb() else {
        eprintln!("note: no lldb: skipping");
        return;
    };
    let dir = std::env::temp_dir().join(format!("ppl-debug-vars-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let main = r#"struct Point {
  x: i32
  y: i32
}
class Person {
  constructor(public name: string, public age: i64) {}
}
function inspect(n: i64, ok: bool, s: string, xs: i64[], p: Point, who: Person, maybe: Person?) {
  const total = n + xs.length
  println(`${total} ${ok} ${s} ${p.x} ${who.name} ${maybe == null}`)
}
inspect(41, true, "hello", [1, 2, 3], Point { x: 3, y: 4 }, new Person("Ann", 30), null)
"#;
    let entry = dir.join("main.ppl");
    std::fs::write(&entry, main).unwrap();
    let checked = ppl_driver::check_source_with(
        entry.display().to_string(),
        main.to_string(),
        &dir,
        BuildOptions::default(),
    );
    assert!(
        !checked.diags.iter().any(|d| d.is_error()),
        "{}",
        checked.render_diagnostics()
    );
    let exe = dir.join("app");
    ppl_driver::build_native(&checked, &exe, BuildOptions::default()).unwrap();
    let out = Command::new(lldb)
        .args([
            "-b",
            "-o",
            "breakpoint set -f main.ppl -l 10",
            "-o",
            "run",
            "-o",
            "frame variable",
        ])
        .args([
            "-o",
            "p who.age",
            "-o",
            "p xs.data[2]",
            "-o",
            "p s.byte_len",
            "-o",
            "kill",
        ])
        .arg(&exe)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    for want in [
        "n = 41",
        "ok = true",
        "x = 3",
        "y = 4",
        "maybe = NULL",
        "total = 44",
        "(long) 30",
        "(long) 3",
        "(long) 5",
    ] {
        assert!(text.contains(want), "{want}: {text}");
    }
}
