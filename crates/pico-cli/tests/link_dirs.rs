//! `pico run -L <dir>` and `pico build -L <dir>` (#113): `@link("adder")` finds `libadder.a` in a
//! directory given on the command line; without it, the build fails and names the library.
#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};
use std::process::Command;

fn on_path(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")?
        .to_str()?
        .split(':')
        .map(|d| Path::new(d).join(name))
        .find(|p| p.is_file())
}

#[test]
fn library_directories_on_the_command_line() {
    let (Some(clang), Some(ar)) = (on_path("clang"), on_path("llvm-ar")) else {
        eprintln!("skipped: needs clang and llvm-ar on PATH");
        return;
    };
    let d = std::env::temp_dir().join(format!("pico-link-dirs-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    let lib = d.join("vendor");
    std::fs::create_dir_all(&lib).unwrap();
    std::fs::write(
        d.join("adder.c"),
        "int add_numbers(int a, int b) { return a + b; }\n",
    )
    .unwrap();
    assert!(
        Command::new(&clang)
            .args(["-c", "-fPIC", "-o"])
            .arg(d.join("adder.o"))
            .arg(d.join("adder.c"))
            .status()
            .unwrap()
            .success()
    );
    assert!(
        Command::new(&ar)
            .arg("rcs")
            .arg(lib.join("libadder.a"))
            .arg(d.join("adder.o"))
            .status()
            .unwrap()
            .success()
    );
    std::fs::write(
        d.join("main.pi"),
        "@link(\"adder\")\nextern \"C\" {\n  function add_numbers(a: c_int, b: c_int): c_int\n}\nunsafe {\n  println(add_numbers(40 as c_int, 2 as c_int))\n}\n",
    )
    .unwrap();
    let pico = env!("CARGO_BIN_EXE_pico");
    let run = Command::new(pico)
        .args(["run", "-L", "vendor", "main.pi"])
        .current_dir(&d)
        .output()
        .unwrap();
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&run.stdout), "42\n");
    let build = Command::new(pico)
        .args(["build", "-L", "vendor", "main.pi", "-o", "main"])
        .current_dir(&d)
        .status()
        .unwrap();
    assert!(build.success());
    let out = Command::new(d.join("main")).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "42\n");
    // Without the directory the library isn't found, and the message names it.
    let bad = Command::new(pico)
        .args(["build", "main.pi", "-o", "bad"])
        .current_dir(&d)
        .output()
        .unwrap();
    assert!(!bad.status.success());
    let err = String::from_utf8_lossy(&bad.stderr);
    assert!(err.contains("adder"), "{err}");
}
