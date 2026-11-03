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
into a ppl `string` with `string.fromC(p)`, inside `unsafe`.

### Type mapping
| C | ppl |
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

C's other names: `c_char`, `c_uchar`, `c_short`, `c_ushort`, `c_uint`, `c_ulong`,
`c_longlong`, `c_ulonglong`, `c_float`, `c_double` (`c_long` is pointer-sized: on Windows, where
C's `long` has 32 bits, declare `i32`). A callback parameter (`extern "C" (a: ptr<void>) => c_int`)
takes a top-level function by name, which mustn't throw. Pointers convert to other pointee
types with `as`. Extern functions run only in native code (the interpreter panics), and
structs cross by pointer until by-value C structs are implemented. Decision 261.

## 12.2 C: automatic header import
```ts
import * as sdl from "c:SDL2/SDL.h"
import * as m from "c:math.h"
sdl.SDL_Init(sdl.SDL_INIT_VIDEO)
```
The compiler parses the header with an embedded libclang and generates `extern`
declarations, `@pod` structs, enums and `#define` constants that are simple literals.
Function-like macros are skipped. Include paths and link flags come from `project.json`
(`"c": { "include": [...], "link": [...], "pkgConfig": ["sdl2"] }`, found in the program's
directory or above) or pkg-config. `c:./x.h` is a header next to the importing file.

The compiler reads the header through clang's JSON AST (decision 262). Functions that are
variadic or pass structs or unions by value are left out for now, as are bit-fields and unions;
pointers to opaque types are `ptr<void>`, `char *` is `ptr<u8>` and a `const char *` parameter
`string`. Enumerators are `c_int` constants.

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
  converted to and from ppl structs when they cross the boundary.
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
function that calls the ppl closure `f` with its arguments array (event listeners:
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
`ppl build --target wasm32-browser` produces `app.wasm`, `app.js` (ESM glue) and `app.d.ts` for
the exported API. Structs and enums cross the boundary as **plain JS object copies**
(`{ x, y }`, `{ kind: "Circle", r }`), while classes are exposed as JS handle classes wrapping
WasmGC references.

```js
import init from "./app.js"
const app = await init()               // runs the program's top-level code
const html = app.render({ count: 1 })  // @export("js") functions
await app.done                         // the exit code, once nothing is pending
```

By default (the WasmGC backend is opt-in with `PPL_BACKEND=wasmgc` until it's optimised and covers this interop: decision 269), the module uses linear memory with the ppl GC: handles keep
their object alive until `free()` (or `[Symbol.dispose]`, or until the handle is collected).
Numbers, `bool`, `string`, structs and classes cross so far (64-bit integers as JS numbers);
arrays, enums, nullable types and throwing, generic or async functions can't be exported yet
(E0355). The event loop runs on browser timers: the program's top level returns to JavaScript,
and the glue resumes the loop when its next timer is due. Under Node.js, `node app.js args…`
runs the program like an executable. Decision 258.

## 12.4 Exporting to C
```ts
@export("c")
export function ppl_hash(data: ptr<u8>, len: usize): u64 { … }
```
`ppl build --lib shared|static` produces `.so`/`.dylib`/`.dll` or `.a`, plus a generated `.h`.
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
