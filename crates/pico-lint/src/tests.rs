//! Snapshot tests: each rule on a fixture, as `line:col code message` lines.

use super::*;
use std::path::PathBuf;

fn dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("pico-lint-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Lints `text` as `main.pi` (with `files` beside it) under `lint` (`project.json`'s value):
/// the compile errors, then the warnings, one per line.
fn run(name: &str, lint: &str, text: &str, files: &[(&str, &str)]) -> String {
    let d = dir(name);
    std::fs::write(d.join("project.json"), format!("{{ \"lint\": {lint} }}")).unwrap();
    for (f, t) in files {
        std::fs::write(d.join(f), t).unwrap();
    }
    let path = d.join("main.pi");
    std::fs::write(&path, text).unwrap();
    let (cfg, errors) = Config::for_dir(&d);
    assert!(errors.is_empty(), "{errors:?}");
    let (checked, warnings) = lint_file(&path, text, &cfg);
    let mut out = String::new();
    for e in checked.diags.iter().filter(|d| d.is_error()) {
        let at = e.main_span().map(|s| {
            let (l, c) = checked.sm.file(s.file).line_col(s.start);
            format!("{l}:{c} ")
        });
        out.push_str(&format!("ERROR {}{}\n", at.unwrap_or_default(), e.message));
    }
    for w in &warnings {
        let s = w.main_span().unwrap();
        let (l, c) = checked.sm.file(s.file).line_col(s.start);
        let help = w
            .help
            .first()
            .map(|h| format!(" ({h})"))
            .unwrap_or_default();
        out.push_str(&format!(
            "{}:{} {} {}{help}\n",
            l,
            c,
            w.code.unwrap(),
            w.message
        ));
    }
    out
}

const OFF: &str = r#"{ "naming": false, "unused": false }"#;

#[test]
fn naming() {
    let text = r#"struct point_2d {
  X_pos: i32
  y: i32
}
enum color { red, Green }
type meters = f64
const MAX_SIZE = 10
const Bad_Const = 1
let my_var: i32 = 2
function Do_Thing<t>(Some_Param: i32): i32 { Some_Param + my_var }
class Shape {
  static ORIGIN_X: i32 = 0
  public Area(): i32 { 0 }
}
extern "C" {
  function c_abs(x_value: i32): i32
}
println(`${MAX_SIZE} ${Bad_Const} ${Do_Thing<i32>(1)} ${new Shape().Area()}`)
"#;
    insta::assert_snapshot!(run("naming", r#"{ "unused": false }"#, text, &[]));
    assert_eq!(run("naming_off", OFF, text, &[]), "");
}

#[test]
fn unused() {
    let geo =
        "export function dot(a: i32, b: i32): i32 { a * b }\nexport function cross(): i32 { 0 }\n";
    let text = r#"import { dot, cross } from "./geo"
import * as geo from "./geo"

const unusedTop = 1
export const shared = 2

function f(a: i32, b: i32, c: i32): i32 {
  let written = 0
  written = 5
  const _ignored = 1
  const used = a + 1
  used
}

interface Shape { area(scale: i32): i32 }
class Square implements Shape {
  private side: i32 = 2
  private never = 3
  constructor(private size: i32) {}
  public area(scale: i32): i32 { side * 2 }
  private helper(): i32 { 1 }
}
println(`${f(1, 2, 3)} ${dot(1, 2)} ${new Square(1).area(1)}`)
xs(1)
function xs(k: i32) {
  for (const item of [1, 2]) { println(k) }
}
function exempt(v: string | i32): i32 {
  comptime const N: i32 = 4
  try { println(v) } catch (e: Error) { println("failed") }
  match (v) {
    s: string => 1,
    n: i32 => N,
  }
}
class Shown {
  constructor(private label: string) {}
  toString(): string { "shown" }
}
println(`${exempt(1)} ${new Shown("a")}`)
"#;
    insta::assert_snapshot!(run(
        "unused",
        r#"{ "naming": false }"#,
        text,
        &[("geo.pi", geo)]
    ));
    assert_eq!(run("unused_off", OFF, text, &[("geo.pi", geo)]), "");
}

const COMPLEX: &str = r#"function simple(a: i32): i32 { a }

function branches(a: i32, b: bool): i32 {
  if (a > 1 && b) {
    return 1
  } else if (a > 2 || !b) {
    return 2
  }
  for (const x of [1, 2]) { println(x) }
  const k: i32 = a > 3 ? 1 : 2
  const f = (n: i32): i32 => if (n > 0) n else if (n < 0) -n else 0
  k + f(a)
}

class C {
  constructor(a: i32) {
    if (a > 0) { println(1) }
    if (a > 1) { println(2) }
    if (a > 2) { println(3) }
    if (a > 3) { println(4) }
  }
}
println(`${simple(1)} ${branches(1, true)}`)
new C(1)
"#;

#[test]
fn complexity() {
    insta::assert_snapshot!(
        "complexity_default",
        run(
            "cx1",
            r#"{ "naming": false, "unused": false, "complexity": true }"#,
            COMPLEX,
            &[]
        )
    );
    insta::assert_snapshot!(
        "complexity_max",
        run(
            "cx2",
            r#"{ "naming": false, "unused": false, "complexity": { "max": 6 } }"#,
            COMPLEX,
            &[]
        )
    );
    assert_eq!(run("cx3", OFF, COMPLEX, &[]), "");
}

const BLANK: &str = r#"import { x } from "./x"
function first(): i32 { 1 }
function second(): i32 {
  const a: i32 = 1
  if (a > 0) {
    println(a)
  }

  if (a > 1) { println(1) } else if (a > 2) { println(2) }
  const v = if (a > 3) 1 else 2
  // A comment that belongs to the match.
  match (v) {
    1 => println(1),
    _ => println(2),
  }
  a
}
/** Documented. */
function third() {}

class K {
  count = 0
  constructor() {}
  public get double(): i32 { 2 }

  public bump() {}
  public set value(v: i32) { count = v }
}
"#;

#[test]
fn blank_lines() {
    let x = "export const x = 1\n";
    let all = run(
        "bl1",
        r#"{ "naming": false, "unused": false, "blankLines": true }"#,
        BLANK,
        &[("x.pi", x)],
    );
    insta::assert_snapshot!("blank_lines_all", all);
    let some = run(
        "bl2",
        r#"{ "naming": false, "unused": false, "blankLines": { "before": ["if", "constructor"] } }"#,
        BLANK,
        &[("x.pi", x)],
    );
    insta::assert_snapshot!("blank_lines_some", some);
    assert_eq!(run("bl3", OFF, BLANK, &[("x.pi", x)]), "");
}

const DOCS: &str = r#"/** A point. */
export struct Point {
  /** Across. */
  x: i32
  y: i32
}

export function undocumented(): i32 { 1 }

/** Has docs. */
export function documented(): i32 { 1 }

/** @deprecated */
export function onlyTags(): i32 { 1 }

/** Speaks. */
export interface Speaker {
  /** Says something. */
  speak(): string
  listen(): void
}

/** A person. */
export class Person implements Speaker {
  public speak(): string { "hi" }
  /** Listens. */
  public listen() {}
  public wave() {}
  private hidden() {}
  helper() {}
}

export const LIMIT = 3
function internal(): i32 { 1 }
"#;

#[test]
fn docs() {
    insta::assert_snapshot!(run(
        "docs",
        r#"{ "naming": false, "unused": false, "docs": true }"#,
        DOCS,
        &[]
    ));
    assert_eq!(run("docs_off", OFF, DOCS, &[]), "");
}

#[test]
fn configuration() {
    let (c, e) = Config::from_json(None);
    assert_eq!(c, Config::default());
    assert!(e.is_empty());
    let v: serde_json::Value = serde_json::from_str(
        r#"{ "complexity": 0, "blankLines": { "before": ["if", "loop"] }, "docs": "yes", "speed": true }"#,
    )
    .unwrap();
    let (c, e) = Config::from_json(Some(&v));
    assert_eq!(c.blank_lines, Some(vec!["if".to_string()]));
    assert_eq!(c.complexity, None);
    insta::assert_snapshot!(e.join("\n"));
}
