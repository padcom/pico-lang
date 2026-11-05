//! pico's own clang (decision 298): the `pico` executable compiles, assembles and archives C
//! itself, and builds use it.

use std::path::{Path, PathBuf};
use std::process::Command;

fn pico() -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_pico"));
    // (Not an installed clang.)
    c.env_remove("PICO_CLANG");
    c
}

/// A scratch directory for one test.
fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("pico-own-clang-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Whether this pico has its own clang (built against an LLVM with clang's static libraries).
fn has_clang() -> bool {
    let ok = pico().args(["clang", "--version"]).output().is_ok_and(|o| {
        o.status.success() && String::from_utf8_lossy(&o.stdout).contains("clang version")
    });
    if !ok {
        eprintln!("skipped: this pico has no clang of its own");
    }
    ok
}

fn run(c: &mut Command) -> String {
    let out = c.output().unwrap();
    assert!(
        out.status.success(),
        "{c:?} failed:\n{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn compiles_assembles_and_archives() {
    if !has_clang() {
        return;
    }
    let d = scratch("objects");
    std::fs::write(
        d.join("f.c"),
        "#include <stddef.h>\nsize_t twice(size_t x) { return 2 * x; }\n",
    )
    .unwrap();
    std::fs::write(d.join("g.S"), ".globl g\ng:\n  ret\n").unwrap();
    let obj = |name: &str| d.join(name);
    for (src, out, target) in [
        ("f.c", "f.o", "x86_64-linux-gnu"),
        ("g.S", "g.o", "aarch64-linux-gnu"),
        ("f.c", "f.wasm.o", "wasm32-unknown-wasip1"),
    ] {
        run(pico()
            .arg("clang")
            .arg(format!("--target={target}"))
            .arg("-c")
            .arg(d.join(src))
            .arg("-o")
            .arg(obj(out)));
        assert!(std::fs::metadata(obj(out)).unwrap().len() > 0, "{out}");
    }
    let lib = d.join("libf.a");
    run(pico()
        .args(["ar", "rcsD"])
        .arg(&lib)
        .arg(obj("f.o"))
        .arg(obj("g.o")));
    let bytes = std::fs::read(&lib).unwrap();
    assert!(bytes.starts_with(b"!<arch>\n"));
    assert!(bytes.windows(5).any(|w| w == b"twice"), "symbol table");
    let _ = std::fs::remove_dir_all(&d);
}

/// A `c:` import goes through clang's AST dump and macros (`cheader.rs`).
#[test]
fn c_imports() {
    if !has_clang() {
        return;
    }
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/run/ffi/c_math.pi");
    let src = std::fs::read_to_string(&fixture).unwrap();
    let want: Vec<&str> = src
        .lines()
        .filter_map(|l| l.strip_prefix("// expect: "))
        .collect();
    let got = run(pico().arg("run").arg(&fixture));
    assert_eq!(got.lines().collect::<Vec<_>>(), want);
}

/// Linux: the musl libc and the runtime, compiled for another target by pico's clang.
#[cfg(target_os = "linux")]
#[test]
fn cross_builds() {
    if !has_clang() {
        return;
    }
    let d = scratch("cross");
    std::fs::write(d.join("hi.pi"), "println(\"hi\")\n").unwrap();
    for target in ["linux-x64-musl", "wasm32-wasi"] {
        let out = d.join(format!("hi-{target}"));
        run(pico()
            .current_dir(&d)
            .args(["build", "--force", "--target", target, "hi.pi", "-o"])
            .arg(&out));
        assert!(out.is_file(), "{target}");
    }
    let _ = std::fs::remove_dir_all(&d);
}
