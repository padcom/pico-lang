//! Runs every fixture in `tests/run` and `tests/fail`.
//! Set `PPL_TEST_FILTER=substring` to run a subset.

use std::path::Path;

#[test]
fn golden() {
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
