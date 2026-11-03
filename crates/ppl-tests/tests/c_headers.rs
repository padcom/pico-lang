//! `c:` imports of real C libraries (spec §12.2, decision 262): each `tests/c/<name>/main.ppl`
//! is built natively (with its `project.json`) and run, and its output must be
//! `expected.out`. Skipped where the library isn't installed (`zlib`: Linux with zlib's
//! header and `libz.so`).

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
    if !installed || !ppl_driver::LLD_AVAILABLE {
        eprintln!("note: zlib isn't installed: skipping the c:zlib.h test");
        return;
    }
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/c/zlib");
    let checked = ppl_driver::check_file(&dir.join("main.ppl")).unwrap();
    assert!(!checked.has_errors(), "{}", checked.render_diagnostics());
    let exe = std::env::temp_dir().join(format!("ppl-c-zlib-{}", std::process::id()));
    ppl_driver::build_native(&checked, &exe, ppl_driver::BuildOptions::default()).unwrap();
    let out = std::process::Command::new(&exe).output().unwrap();
    let _ = std::fs::remove_file(&exe);
    let want = std::fs::read_to_string(dir.join("expected.out")).unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), want);
}
