//! Syntax-error fixtures in `tests/fail/parse/*.ppl`.
//!
//! Each fixture lists the expected error codes in order with `// error: E0xxx` lines. The
//! rendered diagnostics are snapshotted, so changes to messages show up in review.

use ppl_diag::{SourceMap, render_all};
use std::path::Path;

#[test]
fn parse_failures() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fail/parse");
    let mut paths: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "ppl"))
        .collect();
    paths.sort();
    assert!(!paths.is_empty(), "no fixtures in {}", dir.display());
    for path in paths {
        let name = path.file_stem().unwrap().to_string_lossy().into_owned();
        let text = std::fs::read_to_string(&path).unwrap();
        let expected: Vec<&str> = text
            .lines()
            .filter_map(|l| l.trim().strip_prefix("// error:"))
            .map(str::trim)
            .collect();
        let mut sm = SourceMap::new();
        let file = sm.add(format!("parse/{name}.ppl"), text.clone());
        let (_, diags) = ppl_syntax::parse_module(file, &text);
        let got: Vec<String> = diags.iter().map(|d| d.code.unwrap().to_string()).collect();
        assert_eq!(got, expected, "error codes for {name}.ppl");
        assert!(
            diags.len() > 1 || name == "unclosed",
            "{name}.ppl should show several errors"
        );
        insta::assert_snapshot!(name, render_all(&diags, &sm));
    }
}
