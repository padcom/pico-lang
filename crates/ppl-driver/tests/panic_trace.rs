//! A panic's report from native debug builds: message, location and the symbolized stack
//! trace, after `finally` blocks ran (snapshot; the trace format matches the interpreter's).

const SRC: &str = r#"
function inner(xs: int[], i: int): int {
  xs[i]
}
function outer(xs: int[]): int {
  try {
    inner(xs, 5)
  } finally {
    println("outer finally")
  }
}
class Box {
  public run(): int {
    try {
      outer([1, 2])
    } finally {
      println("method finally")
    }
  }
}
println("start")
new Box().run()
"#;

/// Hex addresses would differ between runs; none should appear in a symbolized trace.
fn normalize(s: &str) -> String {
    s.split_inclusive('\n')
        .map(|l| match l.find("0x") {
            Some(i) => format!("{}0x…\n", &l[..i]),
            None => l.to_string(),
        })
        .collect()
}

#[test]
#[cfg(target_os = "macos")]
fn native_debug_trace() {
    let checked = ppl_driver::check_source("trace.ppl", SRC);
    assert!(!checked.has_errors(), "{}", checked.render_diagnostics());
    let exe = ppl_driver::build_cached(&checked, ppl_driver::BuildOptions::default()).unwrap();
    let out = std::process::Command::new(exe).output().unwrap();
    assert_eq!(out.status.code(), Some(ppl_driver::PANIC_EXIT_CODE));
    let report = format!(
        "{}---\n{}",
        String::from_utf8_lossy(&out.stdout),
        normalize(&String::from_utf8_lossy(&out.stderr))
    );
    insta::assert_snapshot!(report);
}

#[test]
fn interpreter_trace() {
    let checked = ppl_driver::check_source("trace.ppl", SRC);
    let mut out: Vec<u8> = Vec::new();
    let p = ppl_driver::run_interp(checked.mir.as_ref().unwrap(), &mut out).unwrap_err();
    insta::assert_snapshot!(format!(
        "{}---\n{}",
        String::from_utf8_lossy(&out),
        p.report()
    ));
}
