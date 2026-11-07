# 12. Interop

## 12.1 C: `extern` declarations
```ts
@link("SDL2")
extern "C" {
  function SDL_Init(flags: u32): i32
  function SDL_CreateWindow(title: string, x: i32, y: i32, w: i32, h: i32, flags: u32): ptr<SDL_Window>
  function SDL_Delay(ms: u32): void
}

@pod struct SDL_Rect { x: i32; y: i32; w: i32; h: i32 }
```
### Safety
- An FFI call whose parameter and return types are only primitives, `@pod` structs (by value) and
  `string` is **safe** and can be made anywhere.
- An FFI call that takes or returns `ptr<T>` (or a callback) has to happen inside `unsafe { }`.

### Strings
A `string` parameter in an `extern` declaration is **converted automatically** to a temporary
NUL-terminated UTF-8 copy that stays valid for the duration of the call. A returned `cstr` is copied
into a pico `string` with `string.fromC(p)`, inside `unsafe`.

### Type mapping
| C | pico |
|---|-----|
| `int8_t … uint64_t`, `int`, `long` (target-sized) | `i8 … u64`, `c_int`, `c_long` |
| `float`, `double` | `f32`, `f64` |
| `bool` | `bool` |
| `T*` | `ptr<T>` |
| `struct` | `@pod struct` |
| function pointer | `extern "C" (args) => R` |
| `const char*` (param) | `string` (auto-converted) |
| `size_t`, `ptrdiff_t` | `usize`, `isize` (pointer-sized at the boundary) |
| `void*` | `ptr<void>` |
| `T*` that may be `NULL` | `ptr<T>?` |

C's other names: `c_char`, `c_uchar`, `c_short`, `c_ushort`, `c_uint`, `c_ulong`,
`c_longlong`, `c_ulonglong`, `c_float`, `c_double` (`c_long` is pointer-sized: on Windows, where
C's `long` has 32 bits, declare `i32`). A callback parameter (`extern "C" (a: ptr<void>) => c_int`)
takes a top-level function by name, which mustn't throw. Pointers convert to other pointee
types with `as`. Extern functions run only in native code (the interpreter panics).
Decision 261.

### Structs by value
A `@pod struct` (§9.5) crosses to C by value, as a parameter or a result, laid out like the
C struct with the same fields in the same order (natural alignment); other structs are
E0355. Every target passes them the way its C ABI does (in registers by size and kind,
homogeneous float aggregates, by hidden reference, …): the compiler writes a small C shim
for each such extern, which clang compiles for the target and which pico calls with
pointers to copies. In a struct, `isize`/`usize` stay 64 bits (use `u32` for a 32-bit
`size_t` field on wasm32). A C variadic function, a callback and a C function pointer take structs by pointer only.
```ts
@pod struct DivT { quot: c_int; rem: c_int }
extern "C" {
  function div(n: c_int, d: c_int): DivT
}
const r = div(17 as c_int, 5 as c_int)   // r.quot 3, r.rem 2
```
Decision 330.

### Linking libraries
`@link("name")` on an `extern` block links the C library `name` on every target. The C
library's own parts (`m`, `pthread`, `dl`, …) are always there. The linker looks in the
directories given with `pico build -L <dir>` (or `pico run -L`), then in `project.json`'s
`c.libDirs`, then in the system's (Linux: the target's multiarch directories; a macOS host:
`cc`'s).
```json
{ "c": { "libDirs": { "windows-x64": ["vendor/SDL2/lib/x64"], "macos-arm64": ["vendor/SDL2/macos"] } } }
```
`libDirs` is a list for every target, or a map from target names (aliases or triples) to
lists; relative paths are from the project's directory.
- **Windows**: `name.lib`, `libname.dll.a` or `libname.a` from those directories, else, for a
  system DLL (`user32`, `gdi32`, `ws2_32`, `opengl32`, …), an import library made from
  MinGW's `.def` files; otherwise the build fails, naming the library.
- **WASI**: `libname.a` from those directories.
- **macOS** (cross builds, pico's lld): libSystem's parts are linked already; other libraries
  are `libname.tbd`, `.dylib` or `.a` in those directories (frameworks aren't supported
  yet). Decision 329.

### Null pointers
A `ptr<T>` is never null. A C pointer that may be `NULL` is `ptr<T>?`: the same machine
pointer, with `null` as address 0, so it crosses to C unchanged. Externs take and return it
(and calling them needs `unsafe`, as with `ptr<T>`); `null` is passed for it, a `ptr<T>`
converts to it implicitly, and `p != null` narrows it to `ptr<T>`. `as` converts between
`ptr<T>?` types (null stays null) and from `ptr<T>` to `ptr<U>?`.
```ts
extern "C" {
  function getenv(name: string): ptr<u8>?
  function free(p: ptr<void>?): void
}
unsafe {
  const home = getenv("HOME")
  if (home != null) println(string.fromC(home))
  free(null)
}
```
`ptr<void>` (C's `void *`) has no pointee: `.read()`, `.write(v)` and pointer arithmetic on it
are E0304; convert it first (`p as ptr<u8>`). Decision 326.

### Function pointers
A C function pointer is `extern "C" (…) => R` (a `ptr<(…) => R>` inside, so `as` converts
other pointers to it, and a nullable one may be `NULL`). A top-level pico function passed
where an extern takes one becomes its address (a callback, decision 261). Calling a C
function pointer needs `unsafe`, takes exactly its parameters and uses the C ABI as an extern
call does (`string` arguments as temporary C strings, narrow integers extended).
```ts
type Handler = extern "C" (sig: c_int) => void
extern "C" {
  function signal(sig: c_int, handler: Handler): Handler?
}
function onInterrupt(sig: c_int) {}
unsafe {
  const prev = signal(2 as c_int, onInterrupt)
  if (prev != null) prev(2 as c_int)
}
```
Decision 328.

### Variadic functions
A C variadic function ends its parameter list with a bare `...` (only in `extern "C"`
blocks: pico functions use rest parameters, E0355 otherwise).
```ts
extern "C" {
  function printf(fmt: string, ...): c_int
}
printf("%d %s %.2f\n", 42, "pico", 3.14)
```
The extra arguments may be numbers, `bool`, `string` (converted like a `string` parameter)
and pointers (E0355 otherwise). C's default argument promotions apply: `bool` and integers
narrower than 32 bits are passed as `int`, `f32` as `double`. An unsuffixed integer literal
there is a `c_int` when it fits (a 64-bit integer otherwise), as in C, so `%d` reads it; give
other integers the type the format expects (`%ld` wants a `c_long`). Passing a pointer needs
`unsafe`. C's stdio buffers its output apart from pico's: call `fflush` before mixing them.
A variadic function is only called directly: not used as a function value (E0355) or with
`tailcall` (E0331). Decision 327.

## 12.2 C: automatic header import
```ts
import * as sdl from "c:SDL2/SDL.h"
import * as m from "c:math.h"
sdl.SDL_Init(sdl.SDL_INIT_VIDEO)
```
The compiler parses the header with an embedded libclang and generates `extern`
declarations, `@pod` structs, enums and `#define` constants that are simple literals.
Function-like macros are skipped. Include paths and link flags come from `project.json`
(`"c": { "include": [...], "link": [...], "libDirs": [...], "pkgConfig": ["sdl2"] }`, found in
the program's directory or above) or pkg-config. `c:./x.h` is a header next to the importing file.

The compiler reads the header through clang's JSON AST (decision 262). Variadic functions end
with `...`; functions that pass structs or unions by value are left out for now, as are
bit-fields and unions;
pointers to opaque types are `ptr<void>`, `char *` is `ptr<u8>` and a `const char *` parameter
`string` (`const char *restrict` too). Enumerators are `c_int` constants.

## 12.3 JavaScript (wasm32-browser)
### Typed imports via `.d.ts`
```ts
import { document, console } from "js:dom"           // typed from lib.dom.d.ts
import { marked } from "js:npm/marked"               // typed from the package's .d.ts

const el = document.getElementById("app")!
el.textContent = marked.parse("# Hello")
```
`.d.ts` mapping is **best effort**:
- TS interfaces and classes become nominal `extern class` handles that wrap a JS object.
- Simple object shapes used as **values** (only primitives, arrays and other simple shapes) are
  converted to and from pico structs when they cross the boundary.
- `number` becomes `f64`, `string` becomes `string`, `boolean` becomes `bool`, `T | undefined` and
  `T | null` become `T?`, and `Promise<T>` becomes `Promise<T> throws JsError`.
- `any` and constructs that can't be mapped (conditional/mapped types, overloads that can't be
  resolved) become `JsValue`.
- Every JS call can throw `JsError`.

### Dynamic `JsValue`
```ts
import { js } from "std/js"
const v: JsValue = js.global.get("someLib").call("doThing", 1, "a")
const n = v.as<f64>()          // throws JsError on type mismatch
```
`get`, `set`, `call`, `invoke`, `construct` and `at` work on any value; `asNumber()`,
`asString()` and `asBool()` convert (or throw `JsError`); `JsValue.fn(f)` makes a JavaScript
function that calls the pico closure `f` with its arguments array (event listeners:
`button.addEventListener("click", JsValue.fn((args) => …))`). A `JsValue` keeps its JavaScript
value alive until it's collected.

### What's implemented
`js:dom` is typed from TypeScript's `lib.dom.d.ts`, `js:npm/<package>` from the package's own
declarations. Only named imports work (`import { document } from "js:dom"`): the compiler
generates classes for the imported names, the interfaces they use directly and their bases;
types further away are `JsValue`. Interfaces with type parameters, `Promise<T>` and object shapes
are `JsValue` for now, and function-typed parameters take a `JsValue` (`JsValue.fn(…)`).
Decision 259.

### Exports to JS
```ts
@export("js")
export function render(state: AppState): string { … }
```
`pico build --target wasm32-browser` produces `app.wasm`, `app.js` (ESM glue) and `app.d.ts` for
the exported API. Structs and enums cross the boundary as **plain JS object copies**
(`{ x, y }`, `{ kind: "Circle", r }`), while classes are exposed as JS handle classes wrapping
WasmGC references.

```js
import init from "./app.js"
const app = await init()               // runs the program's top-level code
const html = app.render({ count: 1 })  // @export("js") functions
await app.done                         // the exit code, once nothing is pending
```

By default (the WasmGC backend is opt-in with `PICO_BACKEND=wasmgc` until it's optimised: decisions 269 and 270; it passes class instances as GC references), the module uses linear memory with the pico GC: handles keep
their object alive until `free()` (or `[Symbol.dispose]`, or until the handle is collected).
Numbers, `bool`, `string`, structs and classes cross so far (64-bit integers as JS numbers);
arrays, enums, nullable types and throwing, generic or async functions can't be exported yet
(E0355). The event loop runs on browser timers: the program's top level returns to JavaScript,
and the glue resumes the loop when its next timer is due. Under Node.js, `node app.js args…`
runs the program like an executable. Decision 258.

## 12.4 Exporting to C
```ts
@export("c")
export function pico_hash(data: ptr<u8>, len: usize): u64 { … }
```
`pico build --lib shared|static` produces `.so`/`.dylib`/`.dll` or `.a`, plus a generated `.h`.
Exported signatures may only use FFI-compatible types. A thread calling in from C gets its own
heap, initialized lazily.

That first call runs the program's top-level code on the calling thread (its module instance).
Exported signatures take numbers, `bool` and pointers (`ptr<S>` of a `@pod` struct puts the
struct's definition in the header). So far libraries are built for linux-gnu; a static one is
linked with `-lm -lpthread`. Decision 263.

## 12.5 WASI
`wasm32-wasi` uses WASI preview 2 for fs, clocks, sockets and random. The `std/*` modules are
implemented on top of it. Programs are preview 2 components (`wasm32-wasip2`): wasi-libc's
preview 1 calls go through the standard adapter, sockets straight to `wasi:sockets`; they run with
`wasmtime run -W exceptions=y` (`try` and panics use Wasm exception handling), with network
access granted (`-S inherit-network=y -S allow-ip-name-lookup=y`) when they use it. There are
no threads (no `Worker`, no `watch`) and no `Command`; TLS trusts only `caCerts` there (no system
roots are in reach). Decisions 255–257.
