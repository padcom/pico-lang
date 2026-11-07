//! MIR dump snapshots for the core control-flow shapes.

fn mir(src: &str) -> String {
    let checked = pico_driver::check_source("test.pi", src);
    assert!(
        !checked.has_errors(),
        "unexpected errors:\n{}",
        checked.render_diagnostics()
    );
    pico_mir::dump(checked.mir.as_ref().unwrap())
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

#[test]
fn generic_instances() {
    // One MIR function per instantiation, named with its type arguments.
    insta::assert_snapshot!(mir(r#"
function max<T extends Comparable<T>>(a: T, b: T): T { if (a.compareTo(b) >= 0) a else b }
const i = max(1, 2)
const s = max("a", "b")
const again = max(3, 4)
"#));
}

#[test]
fn constant_folding() {
    insta::assert_snapshot!(mir(r#"
const KB = 1 << 10
const MB = KB * 1024
const ratio = 1.5 * 2.0
const big = KB > 1000 && !false
function scaled(x: i64): i64 { x * (2 + 3) - -KB }
function narrow(): u8 { (KB / 8) as u8 }
println(MB + scaled(1) + narrow() as i64)
println(`${ratio} ${big}`)
"#));
}

/// Strings fold too (#55): concatenation of constants and the string forms of integers, `bool`s
/// and `char`s, also through `const`s.
#[test]
fn string_folding() {
    insta::assert_snapshot!(mir(r#"
const name = "pico"
function greet(): string { "hello, " + name + "!" }
function tag(): string { `${1 + 2}-${true}-${'x'}` }
println(greet())
println(tag())
"#));
}
