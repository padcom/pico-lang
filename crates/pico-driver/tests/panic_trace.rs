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
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn native_debug_trace() {
    let checked = pico_driver::check_source("trace.pi", SRC);
    assert!(!checked.has_errors(), "{}", checked.render_diagnostics());
    let exe = pico_driver::build_cached(&checked, pico_driver::BuildOptions::default()).unwrap();
    let out = std::process::Command::new(exe).output().unwrap();
    assert_eq!(out.status.code(), Some(pico_driver::PANIC_EXIT_CODE));
    let report = format!(
        "{}---\n{}",
        String::from_utf8_lossy(&out.stdout),
        normalize(&String::from_utf8_lossy(&out.stderr))
    );
    insta::assert_snapshot!(report);
}

#[test]
fn interpreter_trace() {
    let checked = pico_driver::check_source("trace.pi", SRC);
    let mut out: Vec<u8> = Vec::new();
    let p = pico_driver::run_interp(checked.mir.as_ref().unwrap(), &mut out).unwrap_err();
    insta::assert_snapshot!(format!(
        "{}---\n{}",
        String::from_utf8_lossy(&out),
        p.report()
    ));
}
