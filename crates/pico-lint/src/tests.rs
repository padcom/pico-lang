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
    // (`explicitReturn` is on by default; only its own tests look at it.)
    let lint = if lint.contains("explicitReturn") {
        lint.to_string()
    } else {
        lint.replacen('{', r#"{ "explicitReturn": false,"#, 1)
            .replace(",}", "}")
            .replace(", }", " }")
    };
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
/// Every rule off, `explicitReturn` too.
const ALL_OFF: &str = r#"{ "naming": false, "unused": false, "explicitReturn": false }"#;

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

const RETURNS: &str = r#"function total(xs: i32[]): i32 {
  let sum: i32 = 0
  for (const x of xs) sum += x
  sum
}
function wrapped(n: i32): i32 {
  const m = n * 2
  (m)
}
function explicit(n: i32): i32 {
  const m = n * 2
  return m
}
function computed(a: i32, b: i32): i32 { a + b }
function label(n: i32): string {
  if (n > 1) return "many"
  "one"
}
function greet(name: string): string {
  `Hello, ${name}`
}
function number(): i32 { 1 }
function flag(): bool { true }
function list(): i32[] { [1, 2] }
function field(p: Pair): i32 { p.a }
function first(xs: i32[]): i32 { xs[0] }
function pick(c: bool): i32 { c ? 1 : 2 }
function called(): i32 { number() }
function branch(c: bool): i32 { if (c) 1 else 2 }
function made(): Pair { new Pair(1) }
class Pair { constructor(public a: i32) {} }
function nothing(n: i32): void {
  println(n)
}
function* gen(): Iterator<i32> { yield 1 }
const area = {
  const w = 3
  w
}
const id = (x: i32) => x
const twice = (x: i32) => {
  const y = x * 2
  y
}
class Counter {
  constructor(private n: i32) {}
  public get value(): i32 { n }
  public set value(v: i32) { n = v }
  public next(): i32 {
    n += 1
    return n
  }
}
const c = new Counter(1)
c.value = 2
println(`${label(1)} ${greet("x")} ${number()} ${flag()} ${list().length} ${field(made())} ${first([1])} ${pick(true)} ${called()} ${branch(true)}`)
println(`${total([1])} ${wrapped(1)} ${explicit(1)} ${computed(1, 2)} ${area} ${id(1)} ${twice(1)} ${c.value} ${c.next()}`)
nothing(1)
for (const g of gen()) println(g)
"#;

#[test]
fn explicit_return() {
    insta::assert_snapshot!(run(
        "explicit_return",
        r#"{ "naming": false, "unused": false, "explicitReturn": true }"#,
        RETURNS,
        &[]
    ));
    assert_eq!(run("explicit_return_off", ALL_OFF, RETURNS, &[]), "");
}

#[test]
fn configuration() {
    let (c, e) = Config::from_json(None);
    assert_eq!(c, Config::default());
    assert!(e.is_empty());
    let v: serde_json::Value = serde_json::from_str(
        r#"{ "complexity": 0, "blankLines": { "before": ["if", "elsewhere"] }, "docs": "yes", "speed": true, "shadows": true }"#,
    )
    .unwrap();
    let (c, e) = Config::from_json(Some(&v));
    assert_eq!(c.blank_lines, Some(vec!["if".to_string()]));
    assert_eq!(c.complexity, None);
    insta::assert_snapshot!(e.join("\n"));
}

const SIZES: &str = r#"function many(a: i32, b: i32, c: i32, d: i32, e: i32): i32 { a + b + c + d + e }
function deep(xs: i32[]): i32 {
  let n: i32 = 0
  for (const x of xs) {
    if (x > 0) {
      while (n < x) { n += 1 }
    }
  }
  return n
}
const nested = [1, 2].map((x: int) => [3].map((y: int) => [4].map((z: int) => x + y + z)))
const a = 1; const b = 2; const c = 3
println(`${many(1, 2, 3, 4, 5)} ${deep([1])} ${nested.length} ${a + b + c} and some more text`)
"#;

#[test]
fn sizes() {
    insta::assert_snapshot!(run(
        "sizes",
        r#"{ "naming": false, "unused": false, "maxParams": true, "maxDepth": true, "maxLines": 5, "maxNestedCallbacks": true, "maxStatementsPerLine": true, "maxLen": { "max": 80 } }"#,
        SIZES,
        &[]
    ));
    assert_eq!(run("sizes_off", ALL_OFF, SIZES, &[]), "");
    assert_eq!(
        run(
            "sizes_loose",
            r#"{ "naming": false, "unused": false, "maxParams": 5, "maxDepth": 3 }"#,
            SIZES,
            &[]
        ),
        ""
    );
}

#[test]
fn overrides() {
    let v: serde_json::Value = serde_json::from_str(
        r#"{ "maxNestedCallbacks": true, "overrides": [{ "files": ["*.test.pi"], "rules": { "maxNestedCallbacks": 4, "shadow": true } }, { "files": "src/**/gen/*.pi", "rules": { "naming": false } }] }"#,
    )
    .unwrap();
    let (c, e) = Config::from_json(Some(&v));
    assert!(e.is_empty(), "{e:?}");
    assert_eq!(c.on("maxNestedCallbacks"), Some(2));
    let t = c.for_path(Path::new("src/math.test.pi"));
    assert_eq!(t.on("maxNestedCallbacks"), Some(4));
    assert_eq!(t.on("shadow"), Some(0));
    let g = c.for_path(Path::new("src/a/b/gen/x.pi"));
    assert!(!g.naming && g.on("maxNestedCallbacks") == Some(2));
    assert!(c.for_path(Path::new("src/math.pi")).naming);
    assert!(config::glob_match("src/**/*.pi", "src/x.pi"));
    assert!(!config::glob_match("src/*.pi", "src/a/x.pi"));
    let bad: serde_json::Value =
        serde_json::from_str(r#"{ "overrides": [{ "rules": {} }, { "files": "*.pi", "rules": { "maxLen": "long" } }], "quotes": "backtick" }"#)
            .unwrap();
    insta::assert_snapshot!(Config::from_json(Some(&bad)).1.join("\n"));
}

const MISTAKES: &str = r#"const limit = 10
function check(limit: i32): i32 {
  limit + 1
  if (limit > 3) {
    return 1
    println("never")
  }
  const s = "value: ${limit}"
  println(s)
  let x = limit
  x = x
  if (x == x) println("same")
  const f = 0.5
  if (f != f) println("NaN")
  if (x > 1) { println(1) } else if (x > 2) { println(2) } else if (x > 1) { println(3) }
  for (let i = 0; i < 10; i--) { break }
  if (x > 100) {}
  return x
}
function* none(): Iterator<i32> { println(1) }
function tryIt(): i32 {
  try { return 1 } catch (e) { throw e } finally { return 2 }
}
function opt(n: i32): i32 { n ?? 0 }
const fns: (() => i32)[] = []
let k: i32 = 0
while (k < 3) {
  fns.push(() => k)
  k += 1
}
const _hidden = 5
function __internal(): i32 { return 1 }
println(__internal())
function _unusedHelper(_skip: i32): i32 { 1 }
println(`${check(1)} ${tryIt()} ${opt(1)} ${fns.length} ${_hidden} ${limit}`)
for (const v of none()) println(v)
const re = /(a+)(?:b)(?<c>c)?/
println(re.test("aab"))
"#;

#[test]
fn mistakes() {
    insta::assert_snapshot!(run(
        "mistakes",
        r#"{ "naming": false, "unused": false, "shadow": true, "unusedExpressions": true, "unreachable": true, "selfCompare": true, "selfAssign": true, "constantBinary": true, "unsafeFinally": true, "requireYield": true, "forDirection": true, "emptyBlock": true, "dupeElseIf": true, "uselessCatch": true, "templateInString": true, "loopFunc": true, "underscoreDangle": true, "preferNamedCaptureGroup": true }"#,
        MISTAKES,
        &[]
    ));
    assert_eq!(
        run("mistakes_off", ALL_OFF, MISTAKES, &[])
            .lines()
            .filter(|l| !l.starts_with("ERROR"))
            .count(),
        0
    );
}

const ASYNC: &str = r#"async function work(): Promise<int> { 1 }
async function risky(): Promise<int> throws Error { await work() }
async function each(xs: int[]): Promise<int> {
  let t = 0
  for (const x of xs) {
    t += await work()
  }
  for await (const y of ticks()) {
    t += await work() + y
  }
  work()
  risky()
  risky().then((v: int) => println(v))
  risky().catch((e: Error) => 0)
  return t
}
async function* ticks(): AsyncIterator<int> { yield 1 }
function legacy(path: string, callback: (s: string) => void) { callback(path) }
const p = new Promise<int>((ok, fail) => ok(1))
p.then((v: int) => Promise.resolve(v + 1))
p.catch((e: Error) => 0)
legacy("x", (s: string) => println(s))
println(await each([1]))
"#;

#[test]
fn asyncs() {
    insta::assert_snapshot!(run(
        "asyncs",
        r#"{ "naming": false, "unused": false, "floatingPromise": true, "awaitInLoop": true, "requireAwait": true, "preferAwait": true, "promiseReturnWrap": true, "promiseParamNames": true }"#,
        ASYNC,
        &[]
    ));
    assert_eq!(
        run("asyncs_off", ALL_OFF, ASYNC, &[])
            .lines()
            .filter(|l| !l.starts_with("ERROR"))
            .count(),
        0
    );
}

const SIMPLIFY: &str = r#"import { readText as readText } from "std/fs"

struct Point { x: i32; y: i32 }

class Base {
  constructor(public n: i32) {}
}

class Child extends Base {
  constructor(n: i32) { super(n) }
}

class Plain {
  private value: i32 = 0
  constructor() {}
  public set v(x: i32) { value = x }
  public other(): i32 { 1 }
  public get v(): i32 { return value }
}

function f(p: Point): string {
  let unchanged = 1
  let total = 0
  total = total + p.y
  const x = p.x
  const { y: y } = p
  const q = Point { x: x, y }
  const label = "total: " + total + "!" + "?"
  if (total > 1) {
    println(1)
  } else {
    if (total > 0) println(2)
  }
  {
    println(3)
  }
  println(Math.pow(2, 3))
  if (3 == total) println(q.x + y + unchanged)
  label
}
const double = function (n: i32): i32 { n * 2 }
const mapped = [1].map(function (n: int): int { n + 1 })
const id = (n: i32) => { return n }
function done(): void {
  println(f(Point { x: 1, y: 2 }))
  return
}
done()
println(`${double(1)} ${mapped.length} ${id(1)} ${new Child(1).n} ${new Plain().other()}`)
"#;

#[test]
fn simplify() {
    insta::assert_snapshot!(run(
        "simplify",
        r#"{ "naming": false, "unused": false, "preferConst": true, "objectShorthand": true, "uselessRename": true, "operatorAssignment": true, "preferTemplate": true, "uselessConcat": true, "arrowBodyStyle": true, "preferArrowCallback": true, "funcStyle": true, "lonelyIf": true, "loneBlocks": true, "uselessReturn": true, "uselessConstructor": true, "preferDestructuring": true, "preferExponentiation": true, "yoda": true, "groupedAccessors": true }"#,
        SIMPLIFY,
        &[]
    ));
    assert_eq!(
        run("simplify_off", ALL_OFF, SIMPLIFY, &[])
            .lines()
            .filter(|l| !l.starts_with("ERROR"))
            .count(),
        0
    );
}

const STYLE: &str = r#"const a = (1)
const b = ((a))
const c = a + b * 2
const ok = a > 1 && b > 1 || c > 1
const list = [
  1,
  2
]
const one = [1, 2,]
if (a > 0)
  println(a)
if (a > 1) {
  println(1)
}
else {
  println(2)
}
const t = a > 1 ? 1
  : 2
const u = a > 1
  ? 1
  : 2
const total = a *
  b
const sum = a +
  b
//no space
/// Doc comments are fine.
const s = "double";
const single = 'it\'s'
println(
  a
)
println(`${b} ${c} ${ok} ${list.length} ${one.length} ${t} ${u} ${total} ${sum} ${s} ${single}`)
"#;

#[test]
fn style() {
    insta::assert_snapshot!(run(
        "style",
        r#"{ "naming": false, "unused": false, "curly": true, "extraParens": true, "mixedOperators": true, "commaDangle": true, "braceStyle": true, "operatorLinebreak": true, "multilineTernary": true, "spacedComment": true, "extraSemi": true, "quotes": true }"#,
        STYLE,
        &[]
    ));
    assert_eq!(run("style_off", ALL_OFF, STYLE, &[]), "");
    insta::assert_snapshot!(
        "style_double_quotes",
        run(
            "style_double",
            r#"{ "naming": false, "unused": false, "quotes": "double" }"#,
            STYLE,
            &[]
        )
    );
}

const DOCTAGS: &str = r#"/**
 * Adds numbers.
 * @param a the first
 * @param {i32} b the second
 * @param c
 * @returns
 */
function add(a: i32, b: i32, d: i32): i32 { a + b + d }

/**
  * Misaligned.
 *  Indented.
 ** Stars.
 *
 *
 * @custom thing
 * @deprecated
 * @returns nothing
 */
function log(): void {}

/// Counts.
/// @yields numbers
function* count(): Iterator<i32> { yield 1 }

/** Starts here.
 * And ends here. */
function swap(x: i32): i32 { x }

/**
 * Inherited.
 *
 * @inheritdoc
 */
function inherited(x: i32): i32 { x }
println(add(1, 2, 3))
"#;

#[test]
fn doc_tags() {
    insta::assert_snapshot!(run(
        "doctags",
        r#"{ "naming": false, "unused": false, "docTags": true, "docFormat": true }"#,
        DOCTAGS,
        &[]
    ));
    assert_eq!(
        run("doctags_off", ALL_OFF, DOCTAGS, &[])
            .lines()
            .filter(|l| !l.starts_with("ERROR"))
            .count(),
        0
    );
}

/// Cases that linting the repository's own programs reported wrongly once.
#[test]
fn dogfood_cases() {
    let text = r#"class Named { constructor(public name: string) {} }
const maybe: Named? = null
println(maybe?.name ?? "none")
function twice(f: (x: int) => int, n: int): int { f(f(n)) }
println(twice(x => x, 10))
const parts = [1].map(f => {
  f + 1
})
function depth(n: int): int { n }
function probe(): void {
  `${depth(1)}`
  println(parts.length)
}
probe()
reactive count = 1
const stop = watchEffect(() => {
  count
  println("ran")
})
stop.dispose()
"#;
    assert_eq!(
        run(
            "dogfood",
            r#"{ "naming": false, "unused": false, "constantBinary": true, "commaDangle": true, "unusedExpressions": true }"#,
            text,
            &[]
        ),
        ""
    );
}

/// Linting stops before MIR (the lint rules don't need it), but still reports the errors in
/// C and JavaScript signatures, which lowering used to find (spec §14.7).
#[test]
fn signature_errors_without_lowering() {
    let d = dir("signatures");
    let path = d.join("main.pi");
    let text = "extern \"C\" {\n  function getenv(name: string): string\n}\n\n@export(\"c\")\nexport function twice(n: int): string {\n  return `${n}`\n}\n";
    std::fs::write(&path, text).unwrap();
    let (checked, _) = lint_file(&path, text, &Config::default());
    assert!(checked.mir.is_none());
    let errors: Vec<String> = checked
        .diags
        .iter()
        .filter(|d| d.is_error())
        .map(|d| d.code.map(|c| c.to_string()).unwrap_or_default())
        .collect();
    assert_eq!(
        errors,
        ["E0355", "E0355"],
        "{}",
        checked.render_diagnostics()
    );
}
