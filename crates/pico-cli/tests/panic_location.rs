//! A panic raised in an imported module or the core library is reported in that file (#3), by
//! both backends. (Golden tests check only the message.)

use std::path::{Path, PathBuf};
use std::process::Command;

/// A scratch directory for one test.
fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("pico-panic-loc-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// `pico run [--interp] main.pi` in `d`: its stderr.
fn stderr_of(d: &Path, interp: bool) -> String {
    let mut c = Command::new(env!("CARGO_BIN_EXE_pico"));
    c.arg("run");
    if interp {
        c.arg("--interp");
    }
    let out = c.arg("main.pi").current_dir(d).output().unwrap();
    assert!(!out.status.success(), "expected a panic");
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn a_panic_in_an_imported_module_names_that_module() {
    let d = scratch("module");
    std::fs::write(
        d.join("lib.pi"),
        "export function boom(): i32 {\n  panic(\"boom from lib\")\n}\n",
    )
    .unwrap();
    std::fs::write(
        d.join("main.pi"),
        "import { boom } from \"./lib\"\nprintln(boom())\n",
    )
    .unwrap();
    for interp in [true, false] {
        let err = stderr_of(&d, interp);
        assert!(
            err.contains("panic: boom from lib\n  at lib.pi:2:3\n"),
            "interp={interp}:\n{err}"
        );
    }
}

#[test]
fn a_panic_in_the_core_library_names_the_core_file() {
    let d = scratch("core");
    std::fs::write(
        d.join("main.pi"),
        "println(\"start\")\nconst v = new Float64Array(-1)\n",
    )
    .unwrap();
    for interp in [true, false] {
        let err = stderr_of(&d, interp);
        assert!(
            err.contains("  at <core>/buffer.pi:") && !err.contains("main.pi:0:0"),
            "interp={interp}:\n{err}"
        );
    }
}
