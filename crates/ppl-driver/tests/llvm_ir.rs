//! LLVM IR snapshots for simple functions (`--emit llvm-ir`).

fn ir(src: &str) -> String {
    let checked = ppl_driver::check_source("test.ppl", src);
    assert!(
        !checked.has_errors(),
        "unexpected errors:\n{}",
        checked.render_diagnostics()
    );
    ppl_codegen::emit_llvm_ir(checked.mir.as_ref().unwrap())
}

#[test]
fn arithmetic_with_overflow_checks() {
    insta::assert_snapshot!(ir(r#"
function add(a: i32, b: i32): i32 { a + b }
function half(a: u64): u64 { a / 2 }
function neg(x: f64): f64 { -x * 2.0 }
"#));
}

#[test]
fn control_flow_and_calls() {
    insta::assert_snapshot!(ir(r#"
function fib(n: i64): i64 {
  if (n < 2) n else fib(n - 1) + fib(n - 2)
}
println(fib(10))
println("done")
"#));
}

#[test]
fn conversions() {
    insta::assert_snapshot!(ir(r#"
function narrow(x: i64): i16 { x as i16 }
function widen(x: i32): f64 { x }
function toInt(x: f64): u8 { x as u8 }
"#));
}
