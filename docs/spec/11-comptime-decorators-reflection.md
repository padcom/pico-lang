# 11. Comptime, decorators & reflection

## 11.1 Compile-time evaluation
Two mechanisms work together:
1. **Automatic constant folding.** Pure expressions over constants are evaluated at compile time
   without any annotation (`const KB = 1 << 10`).
2. **Explicit `comptime`.** Forces evaluation at compile time and enables code generation:
```ts
comptime const CRC_TABLE: [u32; 256] = buildCrcTable()   // runs in the compiler

function vec<comptime N: i32>(): [f32; N] { … }          // comptime parameter

comptime {
  if (target.os == "windows") println("building for windows")  // compile-time log
}

const impl = comptime if (target.arch == "wasm32") wasmImpl else nativeImpl
```
- Comptime code is ordinary ppl, run by an interpreter inside the compiler. It can't do I/O,
  except reading files under the package root (`comptime.readFile`).
- `target` (os, arch, features) and `build` (mode, flags) are available at comptime.
- Types are values at comptime (`comptime T: type`).
- **Limit**: 10 million evaluation steps per top-level comptime evaluation, after which a compile
  error shows the comptime backtrace. Override it with `@comptimeLimit(50_000_000)` on the
  declaration.
- **Diagnostics**: decorators and comptime code report through `decl.error(msg, span?)`,
  `decl.warn(…)` and `decl.note(…)`. These show up as normal compiler diagnostics pointing at the
  decorated source.

## 11.2 Decorators
A decorator is a **comptime function** that receives a description of the declaration it's
attached to. What a decorator can do:

| Capability | Example |
|------------|---------|
| **Add members** | `@derive(Eq, Hash, Ord)`, `@json` (add `toJSON`/`fromJSON`), `@builder` |
| **Wrap functions** | `@memoize`, `@log`, `@route("GET", "/users")` (wrap or register) |
| **Validate** | `@mustUse`, custom lint rules that emit compile errors and warnings |

Decorators **can't** add new top-level declarations to a module. They can attach **hidden
statics** to the declaration they decorate: `decl.addStatic(name, init)` creates private,
once-initialized state (per thread, like module `const`s) that only the generated code can
reference. It returns a reference to splice into templates.
```ts
import { decorator, ClassDecl, FunctionDecl } from "std/meta"

export decorator function memoize(fn: FunctionDecl): FunctionDecl {
  const cache = fn.addStatic("cache", `new Map<u64, ${fn.returnType}>()`)
  fn.wrapBody((args, call) => `
    const key = ${args.hashKey()}
    ${cache}.get(key) ?? ${cache}.set(key, ${call})
  `)
}

export decorator function mustHaveId(c: ClassDecl) {
  if (!c.hasField("id")) c.error("entity classes need an `id` field")
}

@derive(Eq, Hash)
@mustHaveId
class User { constructor(public id: i64, public name: string) {} }
```
Code generation offers two APIs: **quasi-quote templates** (backtick code with `${}` splicing,
parsed and type-checked when the decorator runs) and an **AST builder** (`ast.call(...)`,
`ast.ident(...)`) for precise control.

Decorators are allowed on classes, structs, enums, functions, methods, fields and parameters.

## 11.3 Runtime reflection
Reflection metadata is produced **on demand for the whole program**: the compiler emits it for
exactly those types that can reach a reflection call. Types that are never reflected cost nothing.
```ts
import { reflect } from "std/reflect"
const t = reflect.typeOf(user)
for (const f of t.fields) println(`${f.name}: ${f.type.name} = ${f.get(user):?}`)
```
- `reflect.typeOf<T>()` / `reflect.typeOf(value)`: name, fields, methods, interfaces, base class
  and decorators.
- Private members are visible to reflection only in the declaring module.

## 11.4 JSON
`JSON.parse<T>(text): T throws JsonError` and `JSON.stringify(value)` are generated at **compile
time** for each `T`. The generated code validates shape and types and constructs real nominal
instances, with no runtime reflection involved.
```ts
struct Config { port: u16; host: string; tags: string[]; debug?: bool }
const c = JSON.parse<Config>(text)     // throws JsonError("field 'port': expected u16, got \"x\"")
```
Field renaming and other options are set with decorators (`@json.name("user_id")`).

## 11.5 Deterministic cleanup: `using`
```ts
{
  using lock = mutex.lock()                   // Disposable: dispose() at scope end
  await using f = await fs.open("data.bin")   // File implements AsyncDisposable
  await using conn = await db.connect(url)
  …
}                                      // disposed in reverse order, also when an exception is thrown
```
`Disposable` has `dispose(): void`, and `AsyncDisposable` has `disposeAsync(): Promise<void>`.
`await using` (as in TS 5.2) awaits `disposeAsync()` at scope exit, on every way out, after the
function's result is computed (an async function's promise settles once disposal is done).
It's allowed in async functions and at the entry's top level. Because all I/O is async, files and
sockets are `AsyncDisposable`.
