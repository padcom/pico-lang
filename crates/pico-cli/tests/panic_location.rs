//! A panic raised in an imported module is reported in that file (#3); one raised in the core
//! library or a `std/` module at the program's call into it (#47, decision 322). Both backends.
//! (Golden tests check only the message.)

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
fn a_reflection_panic_names_the_call_site() {
    let d = scratch("reflect");
    std::fs::write(
        d.join("main.pi"),
        "import { reflect } from \"std/reflect\"\nclass Box { constructor(public n: int) {} }\nconst b = new Box(1)\nprintln(reflect.typeOf(b).fields[0].get<string>(b))\n",
    )
    .unwrap();
    for interp in [true, false] {
        let err = stderr_of(&d, interp);
        assert!(
            err.contains("  at main.pi:4:9\n"),
            "interp={interp}:\n{err}"
        );
    }
}

#[test]
fn a_panic_in_the_core_library_names_the_call_site() {
    let d = scratch("core");
    std::fs::write(
        d.join("main.pi"),
        "println(\"start\")\nconst v = new Float64Array(-1)\n",
    )
    .unwrap();
    for interp in [true, false] {
        let err = stderr_of(&d, interp);
        assert!(
            err.contains("  at main.pi:2:11\n"),
            "interp={interp}:\n{err}"
        );
    }
}
