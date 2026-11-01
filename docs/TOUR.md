# A tour of ppl

One example (or a few) for every language feature, in the order of the spec chapters. It's meant
to be read top to bottom, to show how the language looks and feels. The rules themselves live in
[`docs/spec/`](spec/00-overview.md). Each section links to its chapter.

Contents:
[Hello](#1-hello) · [Lexical](#2-lexical-structure) · [Types](#3-types) ·
[Expressions](#4-expressions) · [Patterns](#5-pattern-matching) ·
[Statements](#6-statements-and-bindings) · [Functions](#7-functions) ·
[Classes & interfaces](#8-classes-and-interfaces) · [Errors](#9-errors-and-panics) ·
[Modules](#10-modules-and-packages) · [Memory](#11-memory) · [Concurrency](#12-concurrency) ·
[Comptime & metaprogramming](#13-comptime-decorators-reflection) · [Interop](#14-interop) ·
[Stdlib](#15-standard-library) · [Tooling](#16-tooling) · [Targets](#17-targets)

---

## 1. Hello

```ts
println("Hello, world!")
```

The entry file's top-level code is the program, so no `main` is needed. A slightly bigger script:

```ts
import { readText } from "std/fs"

const text = await readText("notes.txt")        // top-level await, all I/O is async
let count = 0
for (const line of text.split("\n")) {
  if (line.startsWith("#")) count++
}
println(`${count} headings`)
```

---

## 2. Lexical structure
[Spec 01](spec/01-lexical.md)

### Statement termination
Semicolons are optional. A newline ends a statement unless the expression clearly continues.

```ts
const total = items
  .filter(i => i.active)          // a leading `.` continues the expression
  .map(i => i.price)
  .reduce((a, b) => a + b, 0)

const ok = isValid(x) &&
  isAllowed(x)                    // a trailing binary operator continues it

const ready = loaded
  && !failed                      // so does a leading `&&`, `||`, `??`, `*`, `/` …

const a = 1; const b = 2          // `;` separates statements on one line

f
(x)                               // two statements: `f` and `(x)`, unlike JavaScript
```

### Literals
```ts
const dec   = 1_000_000
const hex   = 0xFF
const oct   = 0o17
const bin   = 0b1010
const pi    = 3.14
const tiny  = 1e-9
const half  = .5

const byte  = 10u8                // suffixes pick the type
const small = 300i16
const f     = 1.5f32
const d     = 2f64

const n = 42                      // int (i64) when nothing says otherwise
const x = 4.2                     // number (f64)
const y: u16 = 42                 // the literal takes its type from context

const dq = "double quoted\n"      // escapes, no interpolation
const sq = 'single quoted'        // same as double quotes
const yes = true
const nothing = null

const digits = /\d+(\.\d+)?/g     // regex literal, checked at compile time
const broken = /(abc/              // error: invalid regex (unclosed group)
```

### Characters
```ts
const c: char = 'a'               // a one-character literal is a char where a char is expected
const s = 'a'                     // otherwise it's a string
const e: char = "ab"              // error: more than one character
const first: char = "żółw"[0]     // indexing a string gives a char ('ż')
```

### Template literals and tagged templates
```ts
const name = "Ann"
const greeting = `Hello, ${name}! You have ${messages.length} messages.`

const query = sql`SELECT * FROM users WHERE id = ${id}`    // tagged template
```

### Multiline literals
Tripled delimiters strip the common indentation and the first and last line breaks. Only the
backtick form interpolates.

````ts
const help = """
    Usage: ppl <command>
      build   compile the project
      run     build and run
    """
// "Usage: ppl <command>\n  build   compile the project\n  run     build and run"

const page = ```
    <h1>${title}</h1>
    <p>${body}</p>
    ```
````

### Format specs
A `:` at the top level of `${…}` starts a format spec: `[[fill] align] [sign] [#] [0] [width]
[.precision] [type]`.

```ts
println(`${3.14159:.2f}`)         // 3.14
println(`${42:08}`)               // 00000042
println(`${42:+}`)                // +42
println(`${255:x}`)               // ff
println(`${255:#X}`)              // 0xFF
println(`${5:b}`)                 // 101
println(`${1234.5:e}`)            // 1.2345e3
println(`${"hi":*^6}`)            // **hi**
println(`${"left":<10}|`)         // left      |
println(`${"right":>10}|`)        //      right|
println(`${0.25:.0%}`)            // 25%
println(`${point:?}`)             // Point { x: 1, y: 2 }   (debug representation)
println(`${cond ? a : b}`)        // a `:` inside a ternary isn't a format spec
const bad = `${x:.2q}`            // error: invalid format spec
```

### Comments
```ts
// line comment
/* block comment (not nested) */

/** Doc comment, picked up by `ppl doc`. */
function documented() {}
```

---

## 3. Types
[Spec 02](spec/02-types.md)

### Primitives
```ts
const a: i8 = -128
const b: u64 = 18_446_744_073_709_551_615
const c: f32 = 1.5
const d: isize = -1               // pointer-sized, for FFI and unsafe code
const e: int = 10                 // int = i64; `.length` and indices are int
const f: number = 0.1             // number = f64
const g: bool = true
const h: char = '🙂'
const s: string = "zażółć gęślą jaźń"

println(s.length)                 // 17: code points, not bytes
println(s.bytes().length)         // 26: UTF-8 bytes
println(s[2])                     // ż

const u: unknown = 42             // top type: must be narrowed before use
if (u is i64) println(u + 1)

function fail(msg: string): never { panic(msg) }   // never returns
function log(msg: string): void { println(msg) }
```
There's no `any` and no `undefined`.

### Numeric conversions
```ts
const small: i32 = 10
const wide: i64 = small           // ok: lossless widening
const real: f64 = small           // ok: every i32 fits in f64
const narrow = wide as i16        // explicit narrowing; panics if out of range
const trunc = 3.9 as i32          // 3; an out-of-range float -> int `as` panics
const bad: i32 = wide             // error: i64 -> i32 can lose data, use `as`
```

### Overflow
```ts
const x = 255u8 + 1u8             // panic: integer overflow (debug and release)
const y = 10 / 0                  // panic: division by zero

const w = 255u8.wrappingAdd(1)    // 0
const s = 250u8.saturatingAdd(10) // 255
const c = 255u8.checkedAdd(1)     // null (type u8?)
```

### Nominal typing
```ts
class Meters { constructor(public value: f64) {} }
class Feet   { constructor(public value: f64) {} }

const m: Meters = new Feet(3)     // error: Feet is not Meters, despite the same shape
```

### Nullability
```ts
let nick: string? = null          // string? is string | null
nick = "annie"

const len = nick?.length ?? 0     // optional chaining and nullish coalescing
const sure = nick!                // asserts non-null; panics on null

if (nick != null) {
  println(nick.toUpperCase())     // narrowed to string
}

function greet(user: User?) {
  if (user == null) return
  println(user.name)              // narrowed after the early return
}

let cache: Map<string, i32>? = null
cache ??= new Map()               // assign only if null
```

### Unions and literal types
```ts
type Dir = "north" | "south" | "east" | "west"
type Id = i64 | string

function show(id: Id): string {
  if (typeof id == "string") id else id.toString()
}

function turn(d: Dir): Dir {
  match (d) {
    "north" => "east",
    "east"  => "south",
    "south" => "west",
    "west"  => "north",
  }
}

function describe(x: Dog | Cat): string {
  if (x instanceof Dog) x.bark() else x.meow()    // instanceof narrows
}
```

### Enums
Payload enums are the sum type:
```ts
enum Color { Red, Green, Blue }

enum Shape {
  Circle(r: f64),
  Rect(w: f64, h: f64),
  Empty,
}

const c = Color.Red
const s = Shape.Circle(r: 1)      // payloads take named arguments
const r = Shape.Rect(2, 3)        // or positional ones
```

Discriminated object unions become an anonymous enum:
```ts
type Event =
  | { kind: "click", x: i32, y: i32 }
  | { kind: "key", code: u32 }

function handle(e: Event) {
  if (e.kind == "click") println(`click at ${e.x},${e.y}`)   // narrowed by the discriminant
  else println(`key ${e.code}`)
}
```

### Structs (value types)
```ts
struct Vec3 {
  x: f32
  y: f32
  z: f32
  length(): f32 { Math.sqrt(x * x + y * y + z * z) }   // members are implicit: no `this.`
  scale(k: f32) { x *= k; y *= k; z *= k }
}

struct Size { w: i32; h: i32 }    // `;` separates fields on one line

let a = Vec3 { x: 1, y: 2, z: 2 }
let b = a                         // a copy
b.x = 10
println(a.x)                      // 1: structs are copied on assignment
println(a.length())               // 3
println(a == Vec3 { x: 1, y: 2, z: 2 })   // true: field-wise equality

struct Particle {
  pos: Vec3                       // nested inline, no pointer
  label: string                   // structs may hold GC references
}
const particles: Particle[] = []  // stored inline in the array

@pod struct Vertex { pos: [f32; 3]; uv: [f32; 2] }   // plain data, C layout (FFI, GPU buffers)
```

### Arrays, fixed arrays, tuples, collections
```ts
const xs: i32[] = [1, 2, 3]       // growable, GC-managed
xs.push(4)
const last = xs.pop()
println(xs.length)                // 3
xs[10]                            // panic: index out of bounds

const ys: Array<string> = []      // the same type, spelled out

const grid: [u8; 16] = [0; 16]    // fixed-size inline array (value type)
const pair: [string, i32] = ["age", 42]    // tuple (value type)
println(pair[1])                  // 42 (constant index only)
const [label, value] = pair

const ages = new Map<string, i32>()
ages.set("Ann", 31)
const env: Map<string, string> = { "LANG": "C", HOME: "/home/ann" }   // Map literal
const tags = new Set<string>(["a", "b"])
```

### Equality, hashing, ordering
```ts
struct Point { x: i32; y: i32 }
// Structs, enums and tuples get ==, hashCode and compareTo automatically, field by field.
const seen = new Set<Point>()
seen.add(Point { x: 1, y: 2 })
const sorted = points.sort()      // ordered by x, then y

@derive(Eq, Hash, Ord)
class Version {
  constructor(public major: i32, public minor: i32) {}
}
println(new Version(1, 2) == new Version(1, 2))   // true thanks to @derive
println(new Version(1, 2) < new Version(1, 10))   // true

class Session implements Equatable<Session>, Hashable {
  constructor(public id: string) {}
  public equals(o: Session): bool { id == o.id }
  public hashCode(): u64 { id.hashCode() }
}

class Plain {}
println(new Plain() == new Plain())               // false: identity comparison
```

### Generics
```ts
function max<T extends Comparable<T>>(a: T, b: T): T {
  if (a.compareTo(b) >= 0) a else b
}
max(3, 7)                         // T = int, inferred
max("a", "b")                     // T = string; each instantiation is compiled separately

function sum<T>(xs: T[]): T where T: Add<T> & Zero {
  let acc = T.zero()
  for (const x of xs) acc = acc + x
  acc
}

class Stack<T> {
  private items: T[] = []
  public push(x: T) { items.push(x) }
  public pop(): T? { items.pop() }
}

struct Pair<A, B> { first: A; second: B }

interface Container<T> { get(i: int): T }

class Cache<K extends Hashable, V = string> { … }   // default type parameter
```

### Aliases and distinct types
```ts
type UserId = i64                 // alias: interchangeable with i64
type Meters = distinct f64        // a new nominal type, no runtime cost
type Feet = distinct f64

const m: Meters = 3.0 as Meters
const f: Feet = m                 // error: Meters is not Feet
const raw: f64 = m as f64         // explicit conversion back
```

### Immutability
```ts
const list = [1, 2]
list = []                         // error: const binding
list.push(3)                      // ok: `const` is about the binding, not the object

class Scene {
  public readonly name: string    // the field can't be reassigned (shallow)
  public nodes: Node[] = []

  constructor(name: string) { this.name = name }

  public readonly count(): int { nodes.length }    // checked: doesn't mutate `this`
  public add(n: Node) { nodes.push(n) }
}

function render(scene: immutable Scene) {          // deep read-only view
  println(scene.count())          // ok: readonly method
  scene.nodes.push(x)             // error: mutation through immutable
  scene.add(x)                    // error: add() isn't readonly
}
```

---

## 4. Expressions
[Spec 03](spec/03-expressions.md)

### Everything is an expression
```ts
const grade = if (score > 90) "A" else if (score > 75) "B" else "C"

const area = {
  const w = 3
  const h = 4
  w * h                           // the last expression is the block's value: 12
}

const bonus = if (vip) 100        // no else: bonus is int?, null when !vip

function sign(n: int): int {
  if (n < 0) return -1            // `return` still exits early
  if (n == 0) 0 else 1            // the last expression is the result
}

const nothing = { log("hi"); }    // a block ending in a `;` statement is void
```

### Object literals
```ts
const p = Point { x: 1, y: 2 }    // type named explicitly
const q: Point = { x: 1, y: 2 }   // type known from context
draw({ x: 0, y: 0 })              // parameter type is Point

{                                 // `{` at the start of a statement is always a block
  const tmp = 1
}
```

### Operators
```ts
const pow = 2 ** 3 ** 2           // 512: `**` is right-associative
const bits = (flags & MASK) | 0b100 ^ ~other
const shifted = x << 2 >> 1 >>> 3
const r = 0..10                   // exclusive range
const eq = a == b                 // strict only; there's no ===
const neither = a ?? (b || c)     // `??` needs parens to mix with && / ||
const t = ready ? "go" : "wait"

let i = 0
i++; --i
i += 5; i **= 2; i <<= 1
flag &&= check(); name ||= "anon"; opt ??= fallback

const big = i64.MAX
big++                             // panic: overflow
```

### Operator overloading
```ts
struct Vec2 {
  x: f32
  y: f32
  operator +(o: Vec2): Vec2 { Vec2 { x: x + o.x, y: y + o.y } }
  operator *(k: f32): Vec2 { Vec2 { x: x * k, y: y * k } }
  operator -(): Vec2 { Vec2 { x: -x, y: -y } }               // unary
  operator [](i: i32): f32 { if (i == 0) x else y }
  operator []=(i: i32, v: f32) { if (i == 0) x = v else y = v }
}

let v = Vec2 { x: 1, y: 2 } + Vec2 { x: 3, y: 4 } * 2
v += Vec2 { x: 1, y: 1 }          // derived from `+`
v[0] = -v[1]
```
`<`/`>` come from `compareTo` and `==` from `equals`. `&&`, `||`, `??`, `=` and `.` can't be
overloaded.

---

## 5. Pattern matching
[Spec 03 §3.5](spec/03-expressions.md#35-pattern-matching)

### `match`
```ts
const desc = match (shape) {
  Circle { r } if r > 10 => "big circle",                   // guard
  Circle { r }           => `circle ${r}`,                  // named payload
  c as Rect { w, h }     => `rect ${w}x${h} (${area(c)})`,  // bind the whole value too
  Empty                  => "nothing",
}

const kind = match (status) {
  200 | 201 | 204 => "ok",        // or-pattern
  300..400        => "redirect",  // range pattern
  404             => "not found",
  _               => "other",     // wildcard
}

const label = match (shape) {
  Rect(w, h) if w == h => "square",   // positional payload
  Rect(_, _)           => "rectangle",
  _                    => "round or empty",
}
```

All pattern forms:
```ts
match (value) {
  42                  => "literal",
  "north" | "south"   => "string literals",
  true                => "bool",
  null                => "null",
  n                   => "binding",
  _                   => "wildcard",
}

match (p) {
  Point { x: 0, y }   => `on the y axis at ${y}`,   // struct/class pattern with a nested literal
  Point { x, y: 0 }   => `on the x axis at ${x}`,
  Point { x, y }      => `(${x}, ${y})`,
}

match (list) {
  []                  => "empty",
  [only]              => `one: ${only}`,
  [first, ...rest]    => `${first} and ${rest.length} more`,   // array pattern with rest
}

match (pair) {
  [0, 0]              => "origin",                  // tuple pattern
  [x, 0] | [0, x]     => `on an axis: ${x}`,
  [x, y]              => `${x}, ${y}`,
}

match (u) {                                         // u: unknown
  s: string           => `string ${s}`,             // type-test pattern
  n: i64 if n < 0     => "negative",
  n: i64              => `int ${n}`,
  _                   => "something else",
}
```

A non-exhaustive match is a compile error that lists what's missing:
```ts
match (shape) {                   // error[E0317]: non-exhaustive match: missing `Empty`
  Circle { r } => r,
  Rect { w, h } => w * h,
}
```

### `switch`
Statement style, no fallthrough, `case` takes patterns:
```ts
switch (cmd) {
  case "build" | "b": build()
  case "test":        test()
  case Run { file }:  run(file)
  case (n: i64):      repeat(n)        // a type test needs parentheses in `case`
  default:            usage()
}

const code = switch (result) {    // used as a value, so it must be exhaustive
  case Ok(_):    0
  case Err(e):   { log(e); 1 }
}
```

### `is`
```ts
if (shape is Circle { r }) println(r)       // the binding is in scope where the test holds

if (!(x is string)) return
x.toUpperCase()                             // narrowed to string from here on

while (queue.next() is Job { id }) process(id)
```

---

## 6. Statements and bindings
[Spec 04](spec/04-statements-and-declarations.md)

### Bindings and destructuring
```ts
const a = 1                       // can't be reassigned
let b: i32 = 2                    // can be reassigned
b = 3

const { x, y } = point
const { x: px, ...others } = point                 // rename and rest
const [first, second] = pair
const [head, ...tail] = items
const { port = 8080, host = "localhost" } = opts   // defaults apply when the field is null

let [p, q] = [1, 2]
[p, q] = [q, p]                   // swap
```
There's no `var` and no hoisting. Using a binding before its declaration is an error, except for
top-level functions and types:
```ts
println(double(2))                // ok: top-level functions are visible everywhere
function double(n: int): int { n * 2 }

println(later)                    // error: `later` used before its declaration
const later = 1
```

### Top-level code
```ts
// main.ppl (entry module): behaves like the body of main()
let verbose = false               // `let` is allowed here
function log(msg: string) { if (verbose) println(msg) }   // functions can use it
verbose = args.includes("-v")

// util.ppl (library module): only `const`
const cache = new Map<string, i32>()   // ok: per thread, each worker gets its own
let counter = 0                        // error: module-level `let` in a library module
```

### Loops
```ts
for (let i = 0; i < 10; i++) println(i)
for (const item of items) println(item)
for (const i of 0..10) println(i)               // 0 to 9
for (const i of (0..100).step(5)) println(i)    // 0, 5, …, 95
for (const [k, v] of ages) println(`${k}: ${v}`)
for (const k of ages.keys()) println(k)         // there's no for-in

while (queue.length > 0) process(queue.pop()!)
do { line = readLine() } while (line != null)

for (const n of nums) {
  if (n < 0) continue             // innermost loop only; there are no labels
  if (n > 100) break
  println(n)
}

const found = loop {              // the only loop that produces a value
  const line = readLine() ?? break null
  if (line.startsWith("#")) break line
}
```

### Type declarations
```ts
type Id = i64 | string            // alias
type Meters = distinct f64        // distinct nominal type
struct Vec2 { x: f32; y: f32 }
enum Color { Red, Green, Blue }
class Point { constructor(public x: i32, public y: i32) {} }
interface Shape { area(): f64 }
```

---

## 7. Functions
[Spec 05](spec/05-functions.md)

### Declarations
```ts
function add(a: i32, b: i32): i32 { a + b }     // the last expression is the result

function greet(name: string, greeting = "Hello", suffix?: string): string {
  `${greeting}, ${name}${suffix ?? ""}`         // suffix?: string means string? = null
}

function area(w: f64, h = w): f64 { w * h }     // defaults can use earlier parameters

function inferred(x: i32) { x * 2 }             // return type inferred (i32)

function fact(n: i64): i64 {                    // recursive: the return type is required
  if (n <= 1) 1 else n * fact(n - 1)
}

const sq = (x: f64) => x * x                    // arrow function
const pick = function (a: int, b: int): int { a }   // function expression
```

### Named arguments
Any parameter can be passed by name, after the positional ones:
```ts
greet("Ann", suffix: "!")
greet(name: "Bob", greeting: "Hi")
drawRect(x: 0, y: 0, w: 100, h: 50)
greet(suffix: "?", "Ann")         // error: positional argument after a named one
```

### Rest, spread, destructured parameters
```ts
function sum(...xs: i64[]): i64 { xs.reduce((a, b) => a + b, 0) }
sum(1, 2, 3)
sum(...numbers)

function move(x: i32, y: i32) { … }
const pos: [i32, i32] = [3, 4]
move(...pos)                      // tuple spread into fixed parameters

struct DbOptions { host: string; port: u16? }
function connect({ host, port = 5432 }: DbOptions) { … }
connect({ host: "db.local" })
```

### Overloading
```ts
function len(s: string): int { s.length }
function len<T>(a: T[]): int { a.length }
function len(m: Map<string, i32>): int { m.size }

len("abc")                        // picks the string overload
len([1, 2])                       // picks the generic one

function f(x: i64) {}
function f(x: f64) {}
f(1u8)                            // error: ambiguous, u8 widens to both

function parse(s: string): i32 {}
function parse(s: string): f64 {} // error: overloads can't differ only in return type
```

### Guaranteed tail calls
```ts
function countDown(n: i64, acc: i64): i64 {
  if (n == 0) return acc
  return tailcall countDown(n - 1, acc + n)    // constant stack, or a compile error
}
countDown(10_000_000, 0)
```

### Closures
```ts
let count = 0
const inc = () => { count++ }     // captured `let`: by reference (boxed)
inc(); inc()
println(count)                    // 2

const fns: (() => int)[] = []
for (const i of 0..3) fns.push(() => i)        // captured `const`: by value
println(fns.map(f => f()))        // [0, 1, 2]

function makeCounter(): () => int {
  let n = 0
  () => ++n
}

class Button {
  private clicks = 0
  public attach(el: Element) {
    el.onclick = () => { clicks++ }            // arrows capture `this` lexically
    el.onkeydown = function () { clicks++ }    // error: function expressions can't use `this`
  }
}
```

### Function types
```ts
type Handler = (req: Request) => Response
type Predicate<T> = (x: T) => bool
type Mapper<T, U, E extends Error = never> = (x: T) => U throws E

function apply<T>(xs: T[], keep: Predicate<T>): T[] { xs.filter(keep) }
```

### Generators
```ts
function* fib(): Iterator<i64> {
  let [a, b] = [0, 1]
  loop { yield a; [a, b] = [b, a + b] }
}

for (const n of fib().take(10)) println(n)

const evens = (0..100).filter(n => n % 2 == 0).map(n => n * n)   // lazy adapters
for (const [i, name] of names.enumerate()) println(`${i}: ${name}`)
for (const [a, b] of xs.zip(ys)) println(a + b)

class Tree implements Iterable<i32> {
  public *iterator(): Iterator<i32> { … }   // Iterable<T> has one method: iterator()
}

async function* lines(path: string): AsyncIterator<string> throws IoError { … }
for await (const line of lines("log.txt")) println(line)
```

---

## 8. Classes and interfaces
[Spec 06](spec/06-classes-interfaces.md)

### Classes
```ts
class Animal {
  static count = 0
  static { Animal.count = 0 }                    // static init block

  public readonly id: i64                        // must be assigned in every constructor

  constructor(public name: string, protected age: i32) {   // parameter properties
    id = nextId()
    count++
  }
  constructor(name: string) { this(name, 0) }    // overloaded constructor that delegates

  public get description(): string { `${name} (${age})` }
  public set nickname(v: string) { name = v }

  public virtual speak(): string { "..." }       // can be overridden
  public describe(): string { `${name} says ${speak()}` }   // final: can't be overridden
  helper() {}                                    // no modifier: private
}

class Dog extends Animal {
  constructor(name: string) { super(name, 3) }
  public override speak(): string { "Woof" }     // `override` is required
}

const d: Animal = new Dog("Rex")
println(d.describe())             // Rex says Woof (virtual dispatch)
println(d.description)            // getter
d.nickname = "Rexy"               // setter
println(Animal.count)
println(d instanceof Dog)         // true
println(d)                        // Dog (id = 1, name = "Rex", age = 3): no toString, so the fields
println("a dog: " + d)            // `+` with a string converts the other side
d.helper()                        // error: helper is private
```

### Inheritance rules
```ts
final class Config {}
class MyConfig extends Config {}  // error: Config is final

class Base { public run() {} }
class Child extends Base {
  public override run() {}        // error: Base.run isn't virtual
}

class Other extends Animal {
  public speak(): string { "?" }  // error: missing `override`
}

class Unfinished {
  public x: i32
  constructor() {}                // error: `x` isn't assigned
}
```

### Abstract classes
```ts
abstract class Shape {
  public abstract area(): f64
  public describe(): string { `area ${area():.2f}` }
}

class Square extends Shape {
  constructor(public side: f64) { super() }
  public override area(): f64 { side * side }
}

new Shape()                       // error: Shape is abstract
```

### Interfaces
```ts
interface Shape {
  area(): f64
  perimeter(): f64
  describe(): string { `shape with area ${area():.2f}` }   // default method
}

class Circle implements Shape {
  constructor(public r: f64) {}
  public area(): f64 { Math.PI * r * r }
  public perimeter(): f64 { 2 * Math.PI * r }
}

class Blob {                      // same shape as Shape, but no `implements`
  public area(): f64 { 1 }
  public perimeter(): f64 { 1 }
}
const s: Shape = new Blob()       // error: interfaces are nominal

interface Named { name(): string }
interface Entity extends Named { id(): i64 }          // interface inheritance

interface A { hello(): string { "A" } }
interface B { hello(): string { "B" } }
class C implements A, B {
  public hello(): string { "C" }  // required: conflicting defaults must be overridden
}

struct Money implements Formattable { … }             // structs implement interfaces
enum Token implements Named {
  Ident(text: string), Number(value: f64)
  name(): string { match (this) { Ident { text } => text, Number(_) => "number" } }
}
const n: Named = Token.Ident("x")                     // boxed when used as an interface
```

---

## 9. Errors and panics
[Spec 07](spec/07-errors.md)

### Error classes
```ts
class IoError extends Error {
  constructor(public path: string, msg: string) { super(msg) }
}
class ParseError extends Error {
  constructor(public line: i32) { super(`parse error at ${line}`) }
}
class ConfigError extends Error {}

function load(path: string): string throws IoError {
  if (!exists(path)) throw new IoError(path, "not found")
  …
}
```

### Lightweight throwables
```ts
enum ParseFail implements Throwable {
  Eof,
  Unexpected(ch: char, at: int),
  message(): string {
    match (this) {
      Eof => "unexpected end of input",
      Unexpected { ch, at } => `unexpected '${ch}' at ${at}`,
    }
  }
}

function next(): Token throws ParseFail {
  if (pos >= src.length) throw ParseFail.Eof      // no allocation, no stack trace
  …
}
```

### Checked `throws`
```ts
function readConfig(path: string): Config throws IoError | ParseError {
  const text = readText(path)     // throws IoError
  parseConfig(text)               // throws ParseError
}

function main() {
  readConfig("app.json")          // error: IoError | ParseError not handled or declared
}

function pure(): i32 { 1 }        // no `throws` clause: can't throw

function any(): void throws Error { … }   // `throws Error` covers every subclass

function helper(s: string) { parseInt(s) }   // private: `throws ParseError` is inferred
export function api(s: string): i64 { parseInt(s) }   // error: exported functions must declare it
```

### Throws polymorphism
```ts
// in the stdlib:
//   map<U, E extends Error>(f: (x: T) => U throws E): U[] throws E

const nums = lines.map(l => parseInt(l))   // this call throws ParseError
const lens = lines.map(l => l.length)      // E = never: this call can't throw
```

### `try` / `catch` / `finally`
```ts
const port = try parsePort(s) catch (e: ParseError) 8080      // try is an expression

const config = try {
  readConfig(path)
} catch (e: IoError { path }) {           // a pattern in the catch clause
  println(`cannot read ${path}`)
  Config.default()
} catch (e: ParseError) {
  throw new ConfigError("bad config", cause: e)
} finally {
  log.flush()                             // always runs; its value is ignored
}

function partial(): Config throws ParseError {
  try readConfig(p) catch (e: IoError) Config.default()   // ParseError still propagates
}

const maybe = try? parsePort(s)           // u16?: any error becomes null
const sure = try! parseInt("42")          // a thrown error becomes a panic
```

### Error details
```ts
try {
  load("x")
} catch (e: IoError) {
  println(e.message)
  println(e.cause?.message ?? "no cause")
  println(e.stack)                        // captured when thrown
}
```

### Panics
```ts
panic("unreachable state")
assert(index < len, "index out of range")
const n = maybeNull!                      // panics when null
const arr = [1, 2, 3]
arr[5]                                    // panics: out of bounds
```
Panics unwind and run `finally` blocks, but `catch` can't catch them. On the main thread they end
the process with the message and a stack trace:
```
panic: index out of bounds: the length is 3 but the index is 5
  at main (src/main.ppl:4:1)
```

---

## 10. Modules and packages
[Spec 08](spec/08-modules-packages.md)

```ts
// geometry.ppl
export struct Vec2 { x: f32; y: f32 }
export function dot(a: Vec2, b: Vec2): f32 { a.x * b.x + a.y * b.y }
export const ORIGIN = Vec2 { x: 0, y: 0 }
function helper() { … }                   // module-private

// main.ppl
import { Vec2, dot } from "./geometry"    // extensionless: ./geometry.ppl or ./geometry/index.ppl
import * as geo from "./geometry"
import { dot as dotProduct } from "./geometry"
import { readText } from "std/fs"         // standard library
import { Router } from "padcom/http-router"   // dependency from project.json
import * as m from "c:math.h"             // C header
import { document } from "js:dom"         // JS, wasm32-browser only

export { Vec2 } from "./geometry"         // re-export
```
There's no `export default`.

Initialization cycles across modules are a compile error:
```ts
// a.ppl
import { B } from "./b"
export const A = B + 1                    // error: initialization cycle a -> b -> a
// b.ppl
import { A } from "./a"
export const B = A + 1
```

Globals that need no import: `print`, `println`, `assert`, `panic`, `Math`, `JSON`, `Map`, `Set`,
`Array`, `Promise`, `Error`, plus the Web globals in [§15](#15-standard-library).

### `project.json`
```json
{
  "name": "padcom/http-router",
  "version": "0.3.1",
  "entry": "src/main.ppl",
  "dependencies": {
    "padcom/json-schema": "^1.2.0",
    "sdl": { "git": "https://github.com/someone/ppl-sdl", "tag": "v2.0.0" }
  },
  "devDependencies": {},
  "targets": ["native", "wasm32-browser", "wasm32-wasi"]
}
```

A workspace:
```json
{ "workspace": ["packages/*"] }
```

```sh
ppl add padcom/json-schema@^1.2   # registry
ppl remove padcom/json-schema
ppl publish                       # tag + PR against the GitHub index
```

---

## 11. Memory
[Spec 09](spec/09-memory-gc.md)

### What lives where
```ts
class Node { public next: Node? = null }  // class instances: GC heap
const names = ["a", "b"]                  // arrays and strings: GC heap
const f = () => names.length              // closures: GC heap
const p = Vec2 { x: 1, y: 2 }             // structs, tuples, [T; N], primitives: inline
```

### GC control
```ts
import { gc } from "std/gc"

gc.collect()                      // full collection
gc.collectYoung()                 // nursery only

function frame() {
  gc.pause()                      // no collections in the frame-critical section
  update(); render()
  gc.resume()
}

const s = gc.stats()
println(`heap ${s.heapBytes}, max pause ${s.maxPauseMs:.2f} ms`)
```

### Weak references and finalization
```ts
const ref = new WeakRef(bigObject)
ref.deref()?.use()                        // null once collected

const meta = new WeakMap<Node, string>()
const visited = new WeakSet<Node>()

const registry = new FinalizationRegistry<string>(name => println(`${name} collected`))
registry.register(obj, "obj")             // the callback runs later, on the event loop
```

### Arenas
```ts
const total = arena {
  const parts = text.split(",")           // allocated in the arena
  parts.map(p => p.length).sum()          // an int escapes, which is fine
}                                         // the whole arena is freed here

const leaked = arena {
  text.split(",")                         // error: an arena reference escapes the block
}

unsafe {
  const a = new Arena(capacity: 1 << 20)
  const ps: ptr<Particle> = a.alloc<Particle>(count: 10_000)
  …
  a.free()                                // dangling pointers after this are UB
}
```

### Unsafe code
```ts
unsafe {
  const p = ptr.of(buffer[0])
  (p + 4).write(0xFF as u8)
  const v = (p + 4).read()
  println(sizeof<Vertex>(), alignof<Vertex>())
  const bits = unsafe.cast<u32>(1.0f32)
}

unsafe function poke(p: ptr<u8>, v: u8) { p.write(v) }

ptr.of(x)                                 // error: pointers need `unsafe`

@pod struct Bad { name: string }          // error: @pod structs can't hold references
```

### `--no-gc`
```ts
// ppl build --no-gc
struct Sensor { id: u8; value: f32 }

function report(s: Sensor): StrBuf<64> {
  const out = StrBuf<64>()
  out.append("sensor ")
  out.append(s.id)
  out
}

enum ReadFail implements Throwable { Timeout, Crc(got: u8) }   // only lightweight throwables

const msg = "a" + name                    // error under --no-gc: string concatenation allocates
const objs = new Map()                    // error under --no-gc: class allocation
```

---

## 12. Concurrency
[Spec 10](spec/10-concurrency.md)

### async / await
```ts
async function fetchUser(id: i64): Promise<User> throws HttpError {
  const res = await fetch(`https://api.example.com/users/${id}`)
  await res.json<User>()
}

const u = await fetchUser(1)              // top-level await; rethrows HttpError (checked)

const [a, b] = await Promise.all([fetchUser(1), fetchUser(2)])
const first = await Promise.race([fetchUser(1), fetchUser(2)])
const results = await Promise.allSettled([fetchUser(1), fetchUser(2)])

const p = fetchUser(3)                    // starts running immediately (eager)
doOtherWork()
const user = await p

setTimeout(() => println("later"), 100)
const id = setInterval(tick, 16)
clearTimeout(id)
queueMicrotask(() => println("soon"))
// the process exits once nothing is pending
```

### Workers
```ts
// main.ppl
struct Job { id: i32; data: i64[] }
struct Result { value: i64 }

const w = new Worker<Job, Result>("./job")
w.onmessage = (r: Result) => println(`done: ${r.value}`)
w.onerror = (e: WorkerPanicked) => println(`worker died: ${e.message}`)
w.postMessage(Job { id: 1, data: [1, 2, 3] })   // deep-copied into the worker's heap
w.terminate()

// job.ppl
import { self } from "std/worker"
self.onmessage = (job: Job) => {
  self.postMessage(Result { value: job.data.sum() })
}
```

### Sendable messages and transfers
```ts
w.postMessage(() => 1)                    // error: closures aren't Sendable

const buf = new ArrayBuffer(64 << 20)
w.postMessage(Frame { pixels: buf }, { transfer: [buf] })   // moved, not copied
println(buf.byteLength)                   // 0: detached in the sender
```

### Inline workers and pools
```ts
const w = new Worker((n: i64): i64 => fib(n))   // capture-free function
const r = await w.call(40)

const limit = 10
const bad = new Worker((n: i64) => n + limit)   // error: inline workers can't capture

import { parallelMap, WorkerPool } from "std/worker"
const hashes = await parallelMap(files, (f: string) => sha256File(f), workers: 8)
const pool = new WorkerPool<Job, Result>("./job", size: navigator.hardwareConcurrency)
```

### Cancellation
```ts
const ac = new AbortController()
setTimeout(() => ac.abort(), 5000)

try {
  const res = await fetch(url, { signal: ac.signal })   // throws HttpError | AbortError
} catch (e: AbortError) {
  println("timed out")
}
```

---

## 13. Comptime, decorators, reflection
[Spec 11](spec/11-comptime-decorators-reflection.md)

### Constant folding and `comptime`
```ts
const KB = 1 << 10                        // folded automatically
const BAD = 255u8 + 1u8                   // error: overflow during constant evaluation

function buildCrcTable(): [u32; 256] {
  const t: [u32; 256] = [0; 256]
  for (const i of 0..256) {
    let c = i as u32
    for (const _ of 0..8) c = if ((c & 1) != 0) 0xEDB88320 ^ (c >>> 1) else c >>> 1
    t[i] = c
  }
  t
}
comptime const CRC_TABLE: [u32; 256] = buildCrcTable()   // runs inside the compiler

function zeros<comptime N: i32>(): [f32; N] { [0; N] }   // comptime parameter
const v = zeros<4>()

function describe<comptime T: type>(): string {          // types are comptime values
  T.fields.map(f => f.name).join(", ")
}

comptime {
  if (target.os == "windows") println("building for windows")   // compile-time log
}

const impl = comptime if (target.arch == "wasm32") wasmImpl else nativeImpl

const VERSION = comptime comptime.readFile("VERSION").trim()   // files under the package root

@comptimeLimit(50_000_000)
comptime const PRIMES = sieve(10_000_000)  // raise the 10M step limit
```

### Decorators
```ts
import { ClassDecl, FunctionDecl, ast } from "std/meta"

export decorator function memoize(fn: FunctionDecl): FunctionDecl {
  const cache = fn.addStatic("cache", `new Map<u64, ${fn.returnType}>()`)   // hidden static
  fn.wrapBody((args, call) => `
    const key = ${args.hashKey()}
    const hit = ${cache}.get(key)
    if (hit != null) return hit
    const v = ${call}
    ${cache}.set(key, v)
    v
  `)                                      // quasi-quote template, type-checked on expansion
  fn
}

export decorator function mustHaveId(c: ClassDecl) {
  if (!c.hasField("id")) c.error("entity classes need an `id` field")
}

export decorator function describable(c: ClassDecl) {
  c.addMethod(ast.method("describe", ast.returns(ast.string(c.name))))   // AST builder
}

export decorator function log(fn: FunctionDecl): FunctionDecl {
  if (fn.isAsync) fn.warn("@log on an async function logs only the start")
  fn.wrapBody((args, call) => `
    println(${ast.string(fn.name)})
    ${call}
  `)
}

@derive(Eq, Hash)
@mustHaveId
class User { constructor(public id: i64, public name: string) {} }

@memoize
function fib(n: i64): i64 { if (n < 2) n else fib(n - 1) + fib(n - 2) }

@route("GET", "/users/:id")
async function getUser(req: Request): Promise<Response> { … }

class Form {
  @validate.email public email: string = ""     // field decorator
  public submit(@nonEmpty note: string) { … }   // parameter decorator
}
```

### Runtime reflection
```ts
import { reflect } from "std/reflect"

const t = reflect.typeOf(user)
println(t.name)                           // User
for (const f of t.fields) println(`${f.name}: ${f.type.name} = ${f.get(user):?}`)
for (const m of t.methods) println(m.name)
println(t.interfaces.map(i => i.name))
println(t.decorators.map(d => d.name))   // ["derive", "mustHaveId"]

const u = reflect.typeOf<User>()
u.field("name")!.set(user, "Ann")
```
Metadata is emitted only for types that can reach a reflection call.

### JSON
```ts
struct Config {
  port: u16
  host: string
  tags: string[]
  debug?: bool
  @json.name("user_id") userId: i64
}

const c = JSON.parse<Config>(text)        // throws JsonError
// JsonError: field 'port': expected u16, got "x"

println(JSON.stringify(c))
```

### `using`
```ts
class Lock implements Disposable {
  public dispose() { release() }
}

async function work() {
  using lock = mutex.lock()                   // dispose() at scope end
  await using f = await fs.open("data.bin")   // disposeAsync() at scope end
  await using conn = await db.connect(url)
  …
}                                             // conn, then f, then lock; also when something throws
```

---

## 14. Interop
[Spec 12](spec/12-interop.md)

### C: extern declarations
```ts
@link("SDL2")
extern "C" {
  function SDL_Init(flags: u32): i32
  function SDL_CreateWindow(title: string, x: i32, y: i32, w: i32, h: i32, flags: u32): ptr<SDL_Window>
  function SDL_Delay(ms: u32): void
  function SDL_GetError(): cstr
  function qsort(base: ptr<void>, n: usize, size: usize, cmp: extern "C" (a: ptr<void>, b: ptr<void>) => c_int): void
}

@pod struct SDL_Rect { x: i32; y: i32; w: i32; h: i32 }

SDL_Init(0x20)                            // safe: only primitives
SDL_Delay(16)

unsafe {
  const win = SDL_CreateWindow("game", 0, 0, 800, 600, 0)   // returns ptr<T>: needs unsafe
  const err = string.fromC(SDL_GetError())                  // copy a C string into a ppl string
}

SDL_CreateWindow("x", 0, 0, 1, 1, 0)      // error: pointer-returning FFI call outside unsafe
```

### C: header import
```ts
import * as sdl from "c:SDL2/SDL.h"
import * as m from "c:math.h"

sdl.SDL_Init(sdl.SDL_INIT_VIDEO)
println(m.cbrt(27.0))
```
```json
{ "c": { "include": ["/opt/sdl/include"], "link": ["SDL2"] } }
```

### Export to C
```ts
@export("c")
export function ppl_hash(data: ptr<u8>, len: usize): u64 { … }
```
```sh
ppl build --lib static            # libfoo.a + foo.h
ppl build --lib shared            # .so / .dylib / .dll + foo.h
```

### JavaScript (wasm32-browser)
```ts
import { document, console } from "js:dom"     // typed from lib.dom.d.ts
import { marked } from "js:npm/marked"         // typed from the package's .d.ts

const el = document.getElementById("app")!
el.textContent = marked.parse("# Hello")
console.log("rendered")

import { js } from "std/js"
const v: JsValue = js.global.get("someLib").call("doThing", 1, "a")
const n = v.as<f64>()                         // throws JsError on mismatch

@export("js")
export function render(state: AppState): string { … }
```
```sh
ppl build --target wasm32-browser # app.wasm, app.js (ESM glue), app.d.ts
```
```js
// in JavaScript
import { render } from "./app.js"
document.body.innerHTML = render({ count: 1 })   // structs cross as plain objects
```

---

## 15. Standard library
[Spec 13](spec/13-stdlib.md)

### Web-style globals
```ts
// Web APIs take their standard options object (a struct built from an object literal)
const res = await fetch("https://api.example.com/users", {
  method: "POST",
  headers: new Headers({ "content-type": "application/json" }),   // Map literal
  body: JSON.stringify(newUser),
})
const users = await res.json<User[]>()   // typed, generated like JSON.parse<T>

const url = new URL("https://x.dev/search?q=ppl")
println(url.searchParams.get("q"))

const bytes = new TextEncoder().encode("żółw")
const text = new TextDecoder().decode(bytes)

const copy = structuredClone(state)
const id = crypto.randomUUID()
const t0 = performance.now()
```

### Files, paths, processes
```ts
import { readText, writeText, readDir, open, stat, mkdir } from "std/fs"
import { join, basename, extname } from "std/path"
import { args, env, exit, cwd, Command } from "std/process"

await mkdir(join(cwd(), "out"))
await writeText("out/hello.txt", "hi")
for await (const entry of readDir(".")) {
  if (extname(entry.name) == ".ppl") println(basename(entry.name))
}

await using f = await open("data.bin")
const chunk = await f.read(1024)

const out = await new Command("git", ["status", "--short"], cwd: repo).output()
println(out.stdout)
if (out.code != 0) exit(out.code)

const child = new Command("ffmpeg", ffArgs, stdin: "pipe", stdout: "pipe", env: { "LANG": "C" }).spawn()
await child.stdin.write(bytes)
for await (const chunk of child.stdout) handle(chunk)
const status = await child.status()
child.kill()

const lines = await Command.shell("ls | wc -l").output()   // through the shell, explicitly
```

### HTTP server and networking
```ts
import { serve } from "std/http"

serve({ port: 8080 }, async (req: Request): Promise<Response> => {   // Deno.serve shape
  if (req.url.endsWith("/health")) return new Response("ok")
  Response.json({ hello: "world" })
})

import { listen, connect } from "std/net"
const server = await listen(port: 9000)
for await (const conn of server) conn.readable.pipeTo(conn.writable)   // echo
```

### Streams, encoding, regex, time
```ts
import { TransformStream } from "std/streams"
import { base64, hex } from "std/encoding"
import { Instant, Duration, DateTime } from "std/time"

const upper = new TransformStream<string, string>({ transform: (s, c) => c.enqueue(s.toUpperCase()) })
await source.pipeThrough(upper).pipeTo(sink)

println(base64.encode(bytes), hex.encode(bytes))

const m = "v1.2.3".match(/v(\d+)\.(\d+)\.(\d+)/)
const clean = text.replace(/\s+/g, " ")

const start = Instant.now()
await work()
println(`took ${start.elapsed().millis()} ms`)
const meeting = DateTime.of(2026, 11, 2, 9, 30, zone: "Europe/Warsaw").plus(Duration.hours(1))
```

### Collections, math, strings, arrays
```ts
import { Deque, PriorityQueue, SortedMap, BitSet } from "std/collections"
import { Vec3, Mat4, Quat } from "std/math"

const q = new PriorityQueue<Task>((a, b) => a.priority - b.priority)
const model = Mat4.translation(Vec3 { x: 1, y: 0, z: 0 }) * Mat4.rotation(Quat.identity())

const words = "  a,b ,c  ".trim().split(",").map(s => s.trim())
println("ppl".padStart(5, "*"), "abc".toUpperCase(), "abc".includes("b"))

const nums = [5, 3, 8]
nums.sort()
println(nums.map(n => n * 2).filter(n => n > 6).join(", "))
println(nums.reduce((a, b) => a + b, 0), nums.find(n => n > 4), nums.some(n => n == 3))
println(nums.slice(1), nums.indexOf(8), nums.every(n => n > 0))
```

### Custom formatting
```ts
import { Formattable, FormatSpec } from "std/fmt"

struct Money implements Formattable {
  cents: i64
  format(spec: FormatSpec): string { `$${cents / 100}.${cents % 100:02}` }
}
println(`${Money { cents: 1234 }}`)     // $12.34
```

---

## 16. Tooling
[Spec 14](spec/14-tooling-cli.md)

```sh
ppl                               # REPL
ppl run script.ppl arg1           # JIT and run
ppl build --release               # native executable
ppl build --target wasm32-wasi
ppl test                          # every *.test.ppl
ppl test parser --watch
ppl bench sort
ppl fmt --check
ppl lint
ppl init
ppl doc
ppl lsp                           # used by the VS Code extension
ppl explain E0308
ppl symbolize crash.txt           # resolve a release stack trace
```

### Tests and benchmarks
```ts
// math.test.ppl
import { describe, test, expect, beforeEach, bench } from "std/test"
import { add, load } from "./math"

describe("add", () => {
  beforeEach(() => reset())

  test("adds numbers", () => {
    expect(add(1, 2)).toBe(3)
    expect([add(1, 1)]).toEqual([2])
  })
  test("overflow panics", () => {
    expect(() => add(i64.MAX, 1)).toPanic()
  })
  test("throws on bad input", () => {
    expect(() => parseInt("x")).toThrow(ParseError)
  })
  test("async works", async () => {
    await expect(load("x")).rejects.toThrow(IoError)
  })
})

bench("sort 10k", () => data.slice().sort())
```

### Diagnostics
```
error[E0308]: mismatched types
  ┌─ src/main.ppl:4:19
  │
4 │ const m: Meters = new Feet(3)
  │          ------   ^^^^^^^^^^^ expected `Meters`, found `Feet`
  │          │
  │          expected because of this annotation
  │
  = note: `Meters` and `Feet` have the same shape, but ppl types are nominal
  = help: convert explicitly: `Meters.fromFeet(new Feet(3))`
```

### Formatter options
```json
{ "fmt": { "indent": 2, "lineWidth": 100, "quotes": "double", "semicolons": false } }
```

---

## 17. Targets
[Spec 15](spec/15-targets.md)

```sh
ppl build --target linux-x64              # x86_64-unknown-linux-gnu, glibc 2.31 stubs
ppl build --target x86_64-unknown-linux-gnu.2.35   # pick the glibc version
ppl build --target linux-arm64-musl       # fully static
ppl build --target macos-x64
ppl build --target windows-x64            # MinGW ABI, from any host
ppl build --target wasm32-browser
ppl build --target wasm32-wasi
```

### Platform-specific code
```ts
const sep = comptime if (target.os == "windows") "\\" else "/"

println(`built for ${target.triple} (${build.mode})`)
```
```
storage.ppl          # default
storage.linux.ppl    # used on Linux
storage.windows.ppl  # used on Windows
storage.wasm.ppl     # used on wasm targets
```
```ts
import { save } from "./storage"          // picks the right file for the target

import { readText } from "std/fs"         // error on wasm32-browser: std/fs isn't available there
```
