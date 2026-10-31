# 4. Statements & declarations

## 4.1 Bindings
```ts
const a = 1            // can't be reassigned
let b: i32 = 2         // can be reassigned
const { x, y, ...rest } = point
const [first, second] = pair
const { port = 8080, host = "localhost" } = opts     // defaults apply when the field is null
```
There's no `var` and no hoisting, and every binding is block-scoped. Using a binding before its
declaration is a compile error. Top-level function and type declarations are an exception: they
can be referenced from anywhere in the module.

## 4.2 Top-level code
A program's root module runs top to bottom. Top-level statements in imported modules run once,
in dependency order, before the importer's own code.

**Entry module.** The top-level code of the module passed to `ppl run`/`ppl build` behaves like the
body of `main()`. `let` is allowed there. Those bindings can't be exported, but every function in the entry file
(named `function`s included) can use them as closures, just as in a JS script.

**Library modules.** Every other module's bindings must be `const`. Module-level `let` is a compile error, because there is no
global mutable state; this fits the strictly isolated threads (§10). A `const` holding a mutable
object (e.g. `const cache = new Map()`) is per thread: every worker evaluates module
initializers in its own heap.

## 4.3 Loops
```ts
for (let i = 0; i < n; i++) { … }          // C-style
for (const item of items) { … }            // any Iterable
for (const i of 0..10) { … }               // exclusive range
for (const i of (0..100).step(5)) { … }
for (const [k, v] of map) { … }
while (cond) { … }
do { … } while (cond)

const found = loop {                       // infinite loop; `break value` yields its value
  const line = readLine() ?? break null
  if (line.startsWith("#")) break line
}
```
- `break` and `continue` apply to the innermost loop. **No labels.**
- Only `loop` can produce a value with `break value`. `for`, `while` and `do-while` have type `void`.
- `for-in` doesn't exist. Use `for (const k of obj.keys())`.

## 4.4 Type declarations
```ts
type Id = i64 | string               // alias
type Meters = distinct f64           // new nominal type
struct Vec2 { x: f32; y: f32 }
enum Color { Red, Green, Blue }
class Point { … }
interface Shape { area(): f64 }
```
