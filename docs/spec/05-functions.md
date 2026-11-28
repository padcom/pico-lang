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
- Constructors take the same parameter forms (`new Command("git", args, cwd: repo)`); with
  overloaded constructors, those whose parameter count, defaults and names fit the arguments are
  candidates (the ones taking exactly the positional arguments given first), and among several the
  arguments' types choose, as for overloaded functions (§5.4, decision 335).

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

function move(x: i32, y: i32) { … }
const pos: [i32, i32] = [3, 4]
move(...pos)                     // tuple spread into fixed-arity parameters

function connect({ host, port = 5432 }: DbOptions) { … }   // destructured parameter with default
```

## 5.4 Overloading
Several functions or methods can share a name if their parameter lists differ. The compiler
resolves the call statically from the argument types, and picks the most specific candidate;
between candidates that fit equally well, the one with more bounded type parameters
(`f<T extends object>` over `f<T>`), as long as the arguments meet the bounds (#70).
An integer argument prefers an integer parameter to a float one, so with `f(x: int)` and
`f(x: number)`, `f(n)` for an `i32` `n` calls `f(int)` (decision 317). A function argument
that returns something prefers a parameter that keeps its result to one returning `void`: with
`test(name, fn: () => void)` and `test(name, fn: () => Promise<void>)`, an async lambda takes the
second.
An ambiguous call is a compile error.
```ts
function len(s: string): i64 { s.length }
function len<T>(a: T[]): i64 { a.length }
function len(m: Map<string, i32>): i64 { m.size }
```
Overloads can't differ only in return type. Named arguments take part in resolution. Overloads
can have default parameters, and a call that is actually ambiguous is a compile error.

Methods overload the same way, instance and static ones, in classes and structs (decision 334):
```ts
class Command {
  public name(n: string): Command { cmdName = n; return this }
  public name(): string { return cmdName }
  public option(flags: string, description = "", defaultValue?: string): Command { … }
  public option(flags: string, description: string, parser: (v: string) => int): Command { … }
}
```
The overloads of a method share its visibility and `readonly`. A closure with untyped
parameters (`(v) => v.length`) has no type before an overload is chosen: it fits a function
type that takes as many parameters, and then gets that overload's parameter types.

Overloads work across inheritance (decision 337). A subclass's method with other parameter
types than the inherited ones is one more overload: `Sub.f(string)` doesn't hide `Base.f(int)`.
One with the same parameter types overrides that overload, which has to be `virtual` (or
`abstract`), and says `override`; each overload has its own vtable slot and can be `final` on
its own. Interfaces overload the same way, and a type implements each overload (or gets the
interface's default for it).
```ts
class Base { public virtual g(x: int): string { … }  public virtual g(x: string): string { … } }
class Sub extends Base {
  public override g(x: string): string { … }   // overrides g(string); g(int) is Base's
  public g(x: bool): string { … }              // a third overload
}
```

An overloaded function used as a value needs an expected function type; the overload with its
parameter types is the value (`const f: (x: int) => string = parse`). A class's method used as a
value, `obj.m`, is a **bound method**: a function that calls `m` on that `obj` (virtually, if `m`
is virtual), chosen among overloads the same way (decision 336).

## 5.4.1 Guaranteed tail calls
```ts
function loopUntil(n: i64, acc: i64): i64 {
  if (n == 0) return acc
  return tailcall loopUntil(n - 1, acc + n)    // guaranteed constant stack, otherwise a compile error
}
```
Ordinary calls in tail position may be optimized, but only `tailcall` guarantees it.

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

## 5.5.1 Async lambdas
`async (params) => body` is a lambda returning a promise, as an `async function` does (decision
338). Its `Promise<T>` result type is written (`async (x: int): Promise<int> throws E => …`) or
comes from where it's used:
```ts
const f: (s: string) => Promise<string, Error> = async (s) => { await delay(1); return s + "!" }
it("loads", async () => { expect(await load("a")).toBe("a") })
```
In overload resolution an async lambda fits only function types returning a promise, and a
plain lambda prefers those that don't, so `action(fn)` overloaded for sync and async callbacks
takes either.

## 5.6 Function types
```ts
type Handler = (req: Request) => Response
type Mapper<T, U, E extends Error = never> = (x: T) => U throws E   // see §7
```
A function value fits where a function type with the same parameter types is expected if it
throws less (§7) and returns the same type or, for object references, a subclass (or a class
instance with other type arguments, such as `Promise<T, never>` for `Promise<T, Error>`).
An arrow function may leave out trailing parameters of type `void` that the expected type has
(`delay(10).then(() => "done")` for a `(value: void) => U`, as in TypeScript; decision 323).
A field or struct field holding a function is called like a method, `button.onClick(x)` (or
`(button.onClick)(x)`); a method of the same name wins, and a nullable one (`((x: int) => void)?`)
needs a check first (#331).

## 5.7 Generators
```ts
function* fib(): Iterator<i64> {
  let [a, b] = [0, 1]
  loop { yield a; [a, b] = [b, a + b] }
}
for (const n of fib().take(10)) println(n)
```
Generators compile to stackless state machines, the same machinery as `async`.

A type is iterable with `for-of` when it implements `Iterable<T>`, whose single method is
`iterator(): Iterator<T>`. A generator method is the usual way to write it:
```ts
class Tree implements Iterable<i32> {
  public *iterator(): Iterator<i32> { yield 1 }
}
```
Arrays (`T[]`), strings (`Iterable<char>`), `Map` (`Iterable<[K, V]>`), `Set`, `Iterator<T>` itself,
`URLSearchParams`, `Headers` and the collections are `Iterable`s, so `function total(xs:
Iterable<int>)` takes any of them; an array or a string passed as one is wrapped (no copy). A type
parameter bounded by it (`I extends Iterable<T>`) may be one of them too. `for-of` also walks any
type with an `iterator()` method, declared `Iterable` or not.

Async generators:
```ts
async function* lines(path: string): AsyncIterator<string> throws IoError { … }
for await (const line of lines("log.txt")) println(line)
```
An async generator may `await` and `yield`; it runs lazily, up to the next `yield`, each time
`next(): Promise<T?, E>` is called (`null`: done). Its type is `AsyncIterator<T, E>`, carrying
what it throws like a promise; `for await` rethrows that (checked). `for await` works in async
functions and at the entry's top level, over anything with an
`asyncIterator(): AsyncIterator<T, E>` method (an `AsyncIterator` is its own): the interface
`AsyncIterable<T, E>`, which streams, sockets and listeners, file watchers, a process's output
readers and channels implement too.
