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

## 12.2 C: automatic header import
```ts
import * as sdl from "c:SDL2/SDL.h"
import * as m from "c:math.h"
sdl.SDL_Init(sdl.SDL_INIT_VIDEO)
```
The compiler parses the header with an embedded libclang and generates `extern`
declarations, `@pod` structs, enums and `#define` constants that are simple literals.
Function-like macros are skipped. Include paths and link flags come from `project.json`
(`"c": { "include": [...], "link": [...] }`) or pkg-config.

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

### Exports to JS
```ts
@export("js")
export function render(state: AppState): string { … }
```
`ppl build --target wasm32-browser` produces `app.wasm`, `app.js` (ESM glue) and `app.d.ts` for
the exported API. Structs and enums cross the boundary as **plain JS object copies**
(`{ x, y }`, `{ kind: "Circle", r }`), while classes are exposed as JS handle classes wrapping
WasmGC references.

## 12.4 Exporting to C
```ts
@export("c")
export function ppl_hash(data: ptr<u8>, len: usize): u64 { … }
```
`ppl build --lib shared|static` produces `.so`/`.dylib`/`.dll` or `.a`, plus a generated `.h`.
Exported signatures may only use FFI-compatible types. A thread calling in from C gets its own
heap, initialized lazily.

## 12.5 WASI
`wasm32-wasi` uses WASI preview 2 for fs, clocks, sockets and random. The `std/*` modules are
implemented on top of it.
