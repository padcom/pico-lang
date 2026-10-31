# 8. Modules & packages

## 8.1 Modules
Every `.ppl` file is a module. Declarations are private unless they're `export`ed.
```ts
// geometry.ppl
export struct Vec2 { x: f32; y: f32 }
export function dot(a: Vec2, b: Vec2): f32 { a.x * b.x + a.y * b.y }
function helper() { … }                     // module-private

// main.ppl
import { Vec2, dot } from "./geometry"
import * as geo from "./geometry"
import { dot as dotProduct } from "./geometry"
export { Vec2 } from "./geometry"           // re-export
```
There's no `export default`. Everything is exported by name.

### Specifier resolution
| Specifier | Resolves to |
|-----------|-------------|
| `./x`, `../x` | `x.ppl`, else `x/index.ppl` (extensionless, relative) |
| `std/fs` | standard library module |
| `pkg` / `pkg/sub` | dependency `pkg` from `project.json` |
| `c:SDL2/SDL.h` | C header import (§12) |
| `js:module` | JS import in wasm targets (§12) |

Import cycles are allowed between declarations. A cycle in top-level initialization (`const`
initializers that depend on each other across modules) is a compile error.

## 8.2 Globals and the standard library
A small set of names is **global** and needs no import: `print`, `println`, `assert`, `panic`,
`Math`, `JSON`, `Map`, `Set`, `Array`, `Promise`, `Error` (and the built-in error classes), and the
primitive types.

Everything else comes from `std/…`:
```ts
import { readText, writeText } from "std/fs"
import { join } from "std/path"
import { serve } from "std/http"
```

## 8.3 Packages
A package is a directory containing a `project.json`:
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
- **Sources**: a central ppl registry (`ppl add http-router`) and git URLs (tag, branch or
  commit).
- **Versioning**: semver ranges, resolved into a lockfile (`project.lock`) that pins exact versions
  and content hashes.
- **Duplicates**: semver-compatible requirements unify to a single version, while different major
  versions can coexist in one graph (Cargo-style).
- Packages ship as source and get compiled together with the consumer, which is what
  monomorphization and comptime need.

## Open questions
- Registry name and hosting; namespacing (`scope/name` required?).
- Workspaces / monorepos with several packages?
- Lockfile name: `project.lock`?
