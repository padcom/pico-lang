//! pico and C programs together. `c:` imports of real C libraries (spec §12.2, decision 262):
//! `tests/c/zlib/main.pi` is built natively (with its `project.json`) and run, where zlib's
//! header and `libz.so` are installed (Linux). Libraries for C (spec §12.4, decision 263):
//! `tests/c/export/hash.pi` is built with `--lib static` and `--lib shared` and called by
//! `main.c` (Linux with clang). Outputs must be `expected.out`.

use std::path::Path;

#[test]
fn zlib() {
    let installed = cfg!(target_os = "linux")
        && Path::new("/usr/include/zlib.h").is_file()
        && [
            "/usr/lib/aarch64-linux-gnu/libz.so",
            "/usr/lib/x86_64-linux-gnu/libz.so",
        ]
        .iter()
        .any(|p| Path::new(p).exists());
    if !installed || !pico_driver::LLD_AVAILABLE {
        eprintln!("note: zlib isn't installed: skipping the c:zlib.h test");
        return;
    }
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/c/zlib");
    let checked = pico_driver::check_file(&dir.join("main.pi")).unwrap();
    assert!(!checked.has_errors(), "{}", checked.render_diagnostics());
    let exe = std::env::temp_dir().join(format!("pico-c-zlib-{}", std::process::id()));
    pico_driver::build_native(&checked, &exe, pico_driver::BuildOptions::default()).unwrap();
    let out = std::process::Command::new(&exe).output().unwrap();
    let _ = std::fs::remove_file(&exe);
    let want = std::fs::read_to_string(dir.join("expected.out")).unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), want);
}

/// `clang` (on `PATH` after `scripts/env.sh`), if it runs.
fn clang() -> Option<&'static str> {
    std::process::Command::new("clang")
        .arg("--version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|_| "clang")
}

#[test]
fn library_for_c() {
    let Some(cc) = clang().filter(|_| cfg!(target_os = "linux") && pico_driver::LLD_AVAILABLE)
    else {
        eprintln!("note: needs Linux, lld and clang: skipping the --lib test");
        return;
    };
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/c/export");
    let checked = pico_driver::check_file(&dir.join("hash.pi")).unwrap();
    assert!(!checked.has_errors(), "{}", checked.render_diagnostics());
    let out = std::env::temp_dir().join(format!("pico-c-export-{}", std::process::id()));
    std::fs::create_dir_all(&out).unwrap();
    let want = std::fs::read_to_string(dir.join("expected.out")).unwrap();
    for (kind, lib) in [
        (pico_driver::LibKind::Static, "libhash.a"),
        (pico_driver::LibKind::Shared, "libhash.so"),
    ] {
        let opts = pico_driver::BuildOptions::default();
        pico_driver::build_library(&checked, &out.join(lib), opts, kind).unwrap();
        let exe = out.join("main");
        let mut c = std::process::Command::new(cc);
        c.arg(dir.join("main.c"))
            .arg("-I")
            .arg(&out)
            .arg("-o")
            .arg(&exe);
        match kind {
            pico_driver::LibKind::Static => c.arg(out.join(lib)).args(["-lm", "-lpthread"]),
            pico_driver::LibKind::Shared => c
                .arg(format!("-L{}", out.display()))
                .arg("-lhash")
                .arg(format!("-Wl,-rpath,{}", out.display()))
                .arg("-lpthread"),
        };
        let built = c.output().unwrap();
        assert!(
            built.status.success(),
            "{}",
            String::from_utf8_lossy(&built.stderr)
        );
        let run = std::process::Command::new(&exe).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&run.stdout), want, "{lib}");
    }
    let _ = std::fs::remove_dir_all(&out);
}
