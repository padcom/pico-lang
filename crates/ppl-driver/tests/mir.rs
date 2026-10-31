//! MIR dump snapshots for the core control-flow shapes.

fn mir(src: &str) -> String {
    let checked = ppl_driver::check_source("test.ppl", src);
    assert!(
        !checked.has_errors(),
        "unexpected errors:\n{}",
        checked.render_diagnostics()
    );
    ppl_mir::dump(checked.mir.as_ref().unwrap())
}

#[test]
fn loops() {
    insta::assert_snapshot!(mir(r#"
function sum(n: i64): i64 {
  let total = 0
  for (let i = 0; i < n; i++) {
    if (i % 2 == 0) continue
    total += i
  }
  while (total > 100) total -= 1
  do { total += 1 } while (total < 5)
  total
}
for (const i of 0..3) println(i)
"#));
}

#[test]
fn if_expressions() {
    insta::assert_snapshot!(mir(r#"
function grade(score: i32): i32 {
  const g: i32 = if (score > 90) 1 else if (score > 75) 2 else 3
  const small: i64 = if (score < 0) score else 0
  g
}
function both(a: bool, b: bool): bool { a && !b || b }
"#));
}

#[test]
fn loop_break_value() {
    insta::assert_snapshot!(mir(r#"
function first(limit: i64): i64 {
  let i = 0
  const found = loop {
    i++
    if (i * i > limit) break i
  }
  found
}
function never(): never { panic("boom") }
assert(first(10) == 4, "first")
"#));
}
