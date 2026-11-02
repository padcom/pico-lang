//! Runs every fixture in `tests/run` and `tests/fail`.
//! Set `PPL_TEST_FILTER=substring` to run a subset.

use std::path::Path;

#[test]
fn golden() {
    if std::env::var_os("PPL_TEST_EXPORT").is_some() {
        return; // (see `export`)
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests");
    let filter = std::env::var("PPL_TEST_FILTER").ok();
    let report = ppl_tests::run_all(&root, filter.as_deref());
    let total = report.passed.len() + report.pending.len() + report.failed.len();
    assert!(total > 0, "no fixtures found under {}", root.display());
    if report.failed.is_empty() {
        println!("{}", report.render());
    } else {
        panic!("\n{}", report.render());
    }
}

/// `PPL_TEST_EXPORT=<dir>` with `PPL_TEST_TARGET=<target>`: builds the run fixtures for the
/// target into the directory for `ppl-golden-run` (CI runs them on Windows) instead of running
/// anything.
#[test]
fn export() {
    let Some(out) = std::env::var_os("PPL_TEST_EXPORT") else {
        return;
    };
    let target = std::env::var("PPL_TEST_TARGET").expect("PPL_TEST_EXPORT needs PPL_TEST_TARGET");
    let target = ppl_driver::Target::parse(&target).unwrap();
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests");
    let filter = std::env::var("PPL_TEST_FILTER").ok();
    let (n, failed) = ppl_tests::export_all(&root, filter.as_deref(), target, Path::new(&out));
    for (name, e) in &failed {
        eprintln!("──── BUILD FAILED {name}\n{e}\n");
    }
    println!("exported {n} fixtures for {target}");
    assert!(
        failed.is_empty(),
        "{} fixtures failed to build",
        failed.len()
    );
}
