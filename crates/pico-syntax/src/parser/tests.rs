//! Parser snapshot tests. Each snapshot holds the S-expression dump, followed by rendered
//! diagnostics if there are any.

use crate::dump::dump_module;
use crate::parse_module;
use pico_diag::{SourceMap, render_all};

fn parse(src: &str) -> String {
    let mut sm = SourceMap::new();
    let file = sm.add("test.pi", src);
    let (m, diags) = parse_module(file, src);
    let mut out = dump_module(&m);
    if !diags.is_empty() {
        out.push_str("\n--- diagnostics ---\n");
        out.push_str(&render_all(&diags, &sm));
    }
    out
}

fn ok(src: &str) -> String {
    let out = parse(src);
    assert!(
        !out.contains("--- diagnostics ---"),
        "unexpected errors:\n{out}"
    );
    out
}

// ---- P1.8: expressions ----

#[test]
fn precedence_and_associativity() {
    insta::assert_snapshot!(ok(r##"
1 + 2 * 3
(1 + 2) * 3
a - b - c
a ** b ** c
-2 ** 2
a = b = c
x += y *= 2
a ? b : c ? d : e
a || b && c
a ?? b
(a || b) ?? c
a | b ^ c & d
a == b < c
a < b == c
0..n - 1
1 << 2 + 3
x as i32 + 1
x as i64 as f64
!a && b
typeof x == "string"
a instanceof B || c
await f() + 1
"##));
}

#[test]
fn postfix_and_calls() {
    insta::assert_snapshot!(ok(r##"
a.b.c
a?.b?.c()
a!.b
f(1)(2)
xs[0][1]
i++ + ++j
f(a, b: 1, ...rest)
JSON.parse<Config>(text)
zeros<4>()
new Map<string, Array<int>>()
new Worker<Job, Result>("./job")
new Point(1, 2).x
map.get(k).set(1)
sql`SELECT ${id}`
x.y = 1
a < b
a < b > c
"##));
}

#[test]
fn literals_and_templates() {
    insta::assert_snapshot!(ok(r##"
[1, 2, ...rest]
[0; 16]
[]
`a${x}b${y:>4}c`
`outer ${`inner ${z}`}`
const re = /\d+/g
const obj = { x: 1, y }
Point { x: 1, y: 2 }
Pair<int, string> { first: 1, second: "a" }
Shape.Circle { r: 1 }
"##));
}

#[test]
fn arrow_functions() {
    insta::assert_snapshot!(ok(r##"
x => x * 2
(x: f64) => x * x
() => { count++ }
(a, b) => a + b
(n: i64): i64 => fib(n)
async () => await f()
async x => x
function (a: int): int { a }
c ? (a) : b
(a)
"##));
}

#[test]
fn control_flow_expressions() {
    insta::assert_snapshot!(ok(r##"
const grade = if (s > 90) "A" else if (s > 75) "B" else "C"
const bonus = if (vip) 100
const line = readLine() ?? break null
return
return x + 1
throw new Error("x")
const port = try parsePort(s) catch (e: ParseError) 8080
const m = try? parsePort(s)
const n = try! parseInt("42")
yield a
yield* inner()
return tailcall count(n - 1)
comptime if (target.os == "windows") a else b
comptime.readFile("x")
unsafe.cast<u32>(x)
const t = arena { parts.length }
"##));
}

// ---- P1.9: statements and blocks ----

#[test]
fn statements() {
    insta::assert_snapshot!(ok(r##"
const a = 1
let b: i32 = 2
const { x, y: py, ...rest } = point
const [first, second] = pair
const { port = 8080, host = "localhost" } = opts
let [p, q] = [1, 2]
for (let i = 0; i < n; i++) println(i)
for (;;) {}
for (const item of items) { println(item) }
for (const [k, v] of map) println(k)
for await (const l of lines()) println(l)
while (x > 0) x--
do { x++ } while (x < 10)
const found = loop {
  const line = readLine() ?? break null
  if (line.startsWith("#")) break line
}
using lock = mutex.lock()
await using f = await open("data.bin")
comptime const K: u32 = 1 << 10
"##));
}

#[test]
fn block_vs_object_literal() {
    insta::assert_snapshot!(ok(r##"
{
  const tmp = 1
}
{ x }
const area = {
  const w = 3
  w * h
}
const empty = {}
const shorthand = { x }
const o = {
  a: 1,
  b: 2,
}
const v = { log("hi"); }
"##));
}

#[test]
fn if_and_else_across_lines() {
    insta::assert_snapshot!(ok(r##"
if (x)
  foo()
else
  bar()
if (a) {
  b()
}
else if (c) {
  d()
}
if (x) for (const i of xs) f(i)
"##));
}

// ---- P1.10: declarations and types ----

#[test]
fn types() {
    insta::assert_snapshot!(ok(r##"
type A = Map<string, Array<int>>
type B = (A | B)[]
type C = string?[]
type D = [f32; 4]
type E = [string, i32]
type F = (x: T, y?: U, ...z: V[]) => W throws E
type G = "north" | "south"
type H = { kind: "click", x: i32 } | { kind: "key", code: u32 }
type I = immutable Scene
type J = distinct f64
type K = Add<T> & Zero
type L = extern "C" (a: ptr<void>, b: ptr<void>) => c_int
type M =
  | A
  | B
"##));
}

#[test]
fn functions_and_params() {
    insta::assert_snapshot!(ok(r##"
function add(a: i32, b: i32): i32 { a + b }
function greet(name: string, greeting = "Hello", suffix?: string): string {}
function sum(...xs: i64[]): i64 {}
function connect({ host, port = 5432 }: DbOptions) {}
function max<T extends Comparable<T>>(a: T, b: T): T { a }
function s<T>(xs: T[]): T where T: Add<T> & Zero { xs[0] }
function z<comptime N: i32>(): [f32; N] {}
async function f(id: i64): Promise<User> throws HttpError {}
function* fib(): Iterator<i64> {}
unsafe function poke(p: ptr<u8>, v: u8) {}
export decorator function memoize(fn: FunctionDecl): FunctionDecl {}
"##));
}

#[test]
fn classes_structs_interfaces() {
    insta::assert_snapshot!(ok(r##"
struct Vec2 { x: f32; y: f32 }
@pod struct V { pos: [f32; 3] }
struct Vec3 {
  x: f32
  length(): f32 { x }
  operator +(o: Vec3): Vec3 { o }
  operator -(): Vec3 { this }
}
abstract class Shape {
  public abstract area(): f64
}
final class Config {}
class Animal {
  static count = 0
  static { Animal.count = 0 }
  public readonly id: i64
  private items: T[] = []
  constructor(public name: string, protected age: i32) { count++ }
  constructor(name: string) { this(name, 0) }
  public get description(): string { name }
  public set nickname(v: string) { name = v }
  public virtual speak(): string { "..." }
  helper() {}
  @validate.email public email: string = ""
  public submit(@nonEmpty note: string) {}
  public readonly count(): int { 1 }
  get(i: int): T { items[i] }
  public *iterator(): Iterator<i32> {}
}
class Dog extends Animal implements Pet, Named {
  public override speak(): string { "Woof" }
}
interface Shape2 extends Named {
  area(): f64
  describe(): string { "x" }
}
"##));
}

#[test]
fn enums_aliases_modules() {
    insta::assert_snapshot!(ok(r##"
enum Color { Red, Green, Blue }
enum Shape {
  Circle(r: f64),
  Rect(w: f64, h: f64),
  Empty,
}
enum ParseFail implements Throwable {
  Eof,
  Unexpected(ch: char, at: int),
  message(): string { "x" }
}
type Id = i64 | string
type Meters = distinct f64
import { a, b as c } from "./x"
import * as geo from "./geometry"
export { Vec2 } from "./geometry"
export { a, b as c }
export * from "./all"
export const ORIGIN = Vec2 { x: 0, y: 0 }
export struct P { x: i32 }
@link("SDL2")
extern "C" {
  function SDL_Init(flags: u32): i32
  function SDL_Delay(ms: u32): void
}
@comptimeLimit(50_000_000)
comptime const PRIMES = sieve(10)
"##));
}

// ---- P1.11: patterns, match, switch, try ----

#[test]
fn match_patterns() {
    insta::assert_snapshot!(ok(r##"
match (v) {
  42 => "lit",
  -1 => "neg",
  "north" | "south" => "or",
  0..10 => "range",
  true => "bool",
  null => "null",
  _ => "wild",
  n => "bind",
  Shape.Circle => "path",
  Circle(r) => "positional",
  Circle { r } if r > 10 => "guard",
  Point { x: 0, y } => "nested literal",
  c as Rect { w, h } => "as",
  [] => "empty",
  [only] => "one",
  [first, ...rest] => "rest",
  [x, 0] | [0, x] => "tuple or",
  s: string => "type test",
  n: i64 if n < 0 => "typed guard",
  Rect(w, h) if w == h => "square",
  (Circle(_) | Empty) => "grouped",
}
const k = match (code) {
  200 | 201 | 204 => "ok"
  300..400 => "redirect"
  _ => "other"
}
"##));
}

#[test]
fn switch_is_and_try() {
    insta::assert_snapshot!(ok(r##"
switch (cmd) {
  case "build" | "b": build()
  case "test":        test()
  case Run { file }:  run(file)
  case (n: i64):      repeat(n)
  case limit:         stop()
  default:            usage()
}
const code = switch (r) {
  case Ok(_): 0
  case Err(e): { log(e); 1 }
}
if (shape is Circle { r }) println(r)
if (!(x is string)) return
while (it.next() is Some(v)) use(v)
const config = try {
  readConfig(path)
} catch (e: IoError { path }) {
  Config.default()
} catch (e: ParseError) {
  throw new ConfigError("bad config", cause: e)
} finally {
  log.flush()
}
try f() catch (e) g()
"##));
}
