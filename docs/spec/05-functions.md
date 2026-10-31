# 5. Functions

## 5.1 Declarations
```ts
function add(a: i32, b: i32): i32 { a + b }         // last expression is the result
function greet(name: string, greeting = "Hello", suffix?: string): string {
  `${greeting}, ${name}${suffix ?? ""}`
}
const sq = (x: f64) => x * x                        // arrow function
```
- Parameter types are required. The return type is inferred when omitted, except on recursive
  and exported functions, which must declare it.
- `p?: T` means `p: T? = null`.
- Defaults can be any expression and can refer to earlier parameters.

## 5.2 Named arguments
**Any** parameter can be passed by name. Positional arguments must come before named ones.
```ts
greet("Ann", suffix: "!")
drawRect(x: 0, y: 0, w: 100, h: 50)
```
That makes parameter names part of a function's public API: renaming an exported function's
parameter is a breaking change.

## 5.3 Rest parameters & spread
```ts
function sum(...xs: i64[]): i64 { xs.reduce((a, b) => a + b, 0) }
sum(1, 2, 3)
sum(...numbers)
```

## 5.4 Overloading
Several functions or methods can share a name if their parameter lists differ. The compiler
resolves the call statically from the argument types, and picks the most specific candidate.
An ambiguous call is a compile error.
```ts
function len(s: string): i64 { s.length }
function len<T>(a: T[]): i64 { a.length }
function len(m: Map<string, i32>): i64 { m.size }
```
Overloads can't differ only in return type. Named arguments take part in resolution.

## 5.5 Closures
Closures capture **by value for `const`** and **by reference for `let`**. The observable
semantics are the same as in JavaScript. A captured `let` is boxed (it lives in a GC cell)
**only** when a closure captures it.
```ts
let count = 0
const inc = () => { count++ }
inc(); inc()
println(count)   // 2
```
Arrow functions capture `this` lexically. Plain `function` expressions inside methods can't use
`this`.

## 5.6 Function types
```ts
type Handler = (req: Request) => Response
type Mapper<T, U> = (x: T) => U throws E        // see §7 on throws polymorphism
```

## 5.7 Generators
```ts
function* fib(): Iterator<i64> {
  let [a, b] = [0, 1]
  loop { yield a; [a, b] = [b, a + b] }
}
for (const n of fib().take(10)) println(n)
```
Generators compile to stackless state machines, the same machinery as `async`.

## Open questions
- Do overloads mix with default parameters, or is that disallowed to avoid ambiguity?
- Are tail calls guaranteed in some form (`return tailcall f()`)?
- Async generators (`async function*` + `for await`)?
