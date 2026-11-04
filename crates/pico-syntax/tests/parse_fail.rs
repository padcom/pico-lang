//! Syntax-error fixtures in `tests/fail/parse/*.pi`.
//!
//! The rendered diagnostics are snapshotted, so changes to messages show up in review. The
//! `// error:` directives themselves are checked by the golden harness (`pico-tests`).

use pico_diag::{SourceMap, render_all};
use std::path::Path;

#[test]
fn parse_failures() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fail/parse");
    let mut paths: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "pi"))
        .collect();
    paths.sort();
    assert!(!paths.is_empty(), "no fixtures in {}", dir.display());
    for path in paths {
        let name = path.file_stem().unwrap().to_string_lossy().into_owned();
        let text = std::fs::read_to_string(&path).unwrap();
        let mut sm = SourceMap::new();
        let file = sm.add(format!("parse/{name}.pi"), text.clone());
        let (_, diags) = pico_syntax::parse_module(file, &text);
        assert!(
            diags.len() > 1 || name == "unclosed",
            "{name}.pi should show several errors"
        );
        insta::assert_snapshot!(name, render_all(&diags, &sm));
    }
}
