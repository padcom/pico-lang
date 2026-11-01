# 11. Comptime, decorators & reflection

## 11.1 Compile-time evaluation
Two mechanisms work together:
1. **Automatic constant folding.** Pure expressions over constants are evaluated at compile time
   without any annotation (`const KB = 1 << 10`).
   Constants flow through `const` bindings and through anything assigned only that constant.
   An operation on constants that would panic at run time (overflow, division by zero, a shift
   out of range, an out-of-range `as`) is a compile error (E0347), even in code that never runs
   (decision 219).
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
  except reading files under the package root (`comptime.readFile`, only inside `comptime`
  code; relative paths that stay inside the root). The clock, the event loop and workers aren't
  available either.
- Comptime code can't use the variables of the code around it (it runs before they exist), nor
  globals that only the running program sets; it can use `comptime const`s declared before it,
  constants, and any function.
- A comptime value becomes part of the program, so it must be data: numbers, strings, `char`,
  `bool`, structs, tuples, fixed and growable arrays, enums and nullables of those. Objects and
  functions aren't allowed (a growable array becomes a fresh array each time its expression
  runs).
- **Comptime parameters**: `function vec<comptime N: i32>()` and `function f(comptime n: i32)`
  take constants (literals, or a comptime parameter of the code around them; anything else is
  E0353). Each distinct constant instantiates the function, like a generic, and the parameter
  is a constant inside it, usable as an array length (`[f32; N]`, never negative) and inferred
  from a fixed-array argument (`sum(xs)` with `xs: [f32; 3]`). For now the parameter's type is
  an integer type, `bool` or `string` (decision 221).
- `comptime if (c) a else b` evaluates only the condition at compile time and keeps only the
  chosen branch, which may be any runtime value (a function, as above).
- `comptime const x = e` is `const x = comptime e`. Output of comptime code is the compile-time
  log, which the compiler prints.
- `target` (`os`, `arch`, `triple`) and `build` (`mode`, `flags`) are compile-time constants,
  usable anywhere (unless the program declares its own names `target`/`build`).
- Types are values at comptime (`comptime T: type`, as a generic or a value parameter:
  `schema(Point)`). A type answers `T.name`, `T.kind` (`"struct"`, `"class"`, `"enum"`,
  `"primitive"`, …), `T.fields` (`FieldInfo { name, typeName }`) and `T.methods`
  (`MethodInfo { name }`); private members are listed only inside the declaring module
  (decision 222).
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
import { ClassDecl, FunctionDecl } from "std/meta"

export decorator function memoize(fn: FunctionDecl): FunctionDecl {
  const cache = fn.addStatic("cache", `new Map<u64, ${fn.returnType}>()`)
  fn.wrapBody((args, call) => `
    const key = ${args.hashKey()}
    const hit = ${cache}.get(key)
    if (hit != null) return hit
    const v = ${call}
    ${cache}.set(key, v)
    v
  `)
  fn
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
`ast.ident(...)`) for precise control. Both produce code text (decision 224), so builder
pieces splice into templates (`${ast.string(fn.name)}`); errors in generated code are
reported where it was generated, with a label at the decorator that generated it.

Decorators are allowed on classes, structs, enums, functions, methods, fields and parameters.

**How they run** (decision 223): before the program is compiled, its decorators run in a
program of their own (the core library, `std/meta`, and the declarations the decorator
functions use), one call per use with a fresh `ClassDecl` (classes and structs) or
`FunctionDecl`, followed by the decorator's arguments (evaluated in the decorator's module).
Their requests apply in order: `addMethod`/`addField`/`addMember(code)` parse a member into
the class, `addStatic(name, init)` adds a module-level hidden `const` (initialized before the
module's other code) and returns its name, `wrapBody(f)` replaces the body with the code `f`
returns, in which `call` runs the original body (`args.hashKey()`, `args.list()` and
`args.names` describe the arguments), and `error`/`warn`/`note` report at the decorator (an
error is E0354). A decorator taking a `ClassDecl` used on a function (or the reverse) is
E0354. Decorator functions and their uses are then removed from the program. For now:
top-level classes, structs and functions only; generated code is parsed as written, so
mistakes in it are reported in a generated file named after the decorator.

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
