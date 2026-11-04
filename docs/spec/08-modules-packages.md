# 8. Modules & packages

## 8.1 Modules
Every `.pi` file is a module. Declarations are private unless they're `export`ed.
```ts
// geometry.pi
export struct Vec2 { x: f32; y: f32 }
export function dot(a: Vec2, b: Vec2): f32 { a.x * b.x + a.y * b.y }
function helper() { … }                     // module-private

// main.pi
import { Vec2, dot } from "./geometry"
import * as geo from "./geometry"
import { dot as dotProduct } from "./geometry"
export { Vec2 } from "./geometry"           // re-export
```
There's no `export default`. Everything is exported by name.

### Specifier resolution
| Specifier | Resolves to |
|-----------|-------------|
| `./x`, `../x` | `x.pi`, else `x/index.pi` (extensionless, relative) |
| `std/fs` | standard library module |
| `scope/name` | the dependency's `index.pi` (§8.3; only the package's root: `scope/name/sub` is an error) |
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
  "entry": "src/main.pi",
  "dependencies": {
    "padcom/json-schema": "^1.2.0",
    "sdl": { "git": "https://github.com/someone/pico-sdl", "tag": "v2.0.0" }
  },
  "devDependencies": {},
  "targets": ["native", "wasm32-browser", "wasm32-wasi"]
}
```
- **Sources**: a central pico registry (`pico add http-router`) and git URLs (tag, branch or
  commit).
- **Versioning**: semver ranges, resolved into a lockfile (`project.lock`) that pins exact versions
  and content hashes.
- **Duplicates**: semver-compatible requirements unify to a single version, while different major
  versions can coexist in one graph (Cargo-style).
- **Names** must be scoped, `scope/name`. Imports use the full name:
  `import { Router } from "padcom/http-router"`.
- **Workspaces**: a root `project.json` with `"workspace": ["packages/*"]` groups several packages
  under one shared lockfile. Members depend on each other by name.
- Packages ship as source and get compiled together with the consumer, which is what
  monomorphization and comptime need.

### Manifest
| Field | |
|-------|---|
| `name` | `scope/name`; required for libraries and workspace members |
| `version` | semver (`1.2.3`); what other packages' ranges match |
| `entry` | an application's main file (`pico init` writes `"src/main.pi"`): what `pico build` and `pico run` without a file, and VS Code's debugger, build |
| `dependencies` | name → a semver range (the registry, §8.4, or a workspace member) or `{ "git": url, "tag" \| "branch" \| "rev": … }` |
| `devDependencies` | the same, visible only to `*.test.pi` files (`pico test`, `pico bench`) |
| `workspace` | in a root manifest: globs of member directories (`["packages/*"]`) |

A package's API is its root `index.pi`: `import { Router } from "padcom/http-router"` loads
`index.pi` of that package, and a package without one exports nothing. Imports reach only the
package's root, not its other files.

### Resolution and the lockfile
- A package is identified by its name and its version's major (the minor for `0.x`, the patch
  for `0.0.x`, as in Cargo). Requirements in one major unify to one copy, which must satisfy
  them all (else it's an error); different majors coexist, each compiled on its own.
- `project.lock` (JSON, at the package's or the workspace's root) pins every package: its
  name, version, source (a git URL and commit, or a workspace path) and content hash
  (`sha256:` over its files, sorted by path), and what each dependency resolved to.
- A git dependency is cloned by commit into the cache (`~/.cache/pico/git`, `PICO_CACHE_DIR`);
  a tag or branch is resolved to a commit once, when the lockfile records it.
- Builds work offline: they read the lockfile and the cache, and fail if a cached package's
  content doesn't match its hash. A missing package is fetched (that needs the network);
  `pico fetch` gets them all ahead of time. A changed manifest updates the lockfile.

### Commands
| Command | |
|---------|---|
| `pico init [name] [--lib]` | a `project.json` and `src/main.pi` (an application with `"entry": "src/main.pi"`), or `index.pi` with `--lib` |
| `pico add <git-url> [--tag t \| --branch b \| --rev c] [--dev]` | adds a git dependency (its name comes from its `project.json`); a registry name (`pico add scope/name`) comes with §8.4 |
| `pico remove <name>` | removes a dependency |
| `pico fetch` | downloads every locked package into the cache |

Decisions 284–286.

## 8.4 Registry
The registry is a **git-backed index hosted on GitHub**, similar to the crates.io index:
- Each package has one index file, `<scope>/<name>.json`, listing its versions. Every version records
  a git URL, a tag, a commit SHA, a content hash and its dependencies.
- `pico publish` checks the package, tags the release and opens a pull request against the index
  (automated checks merge it).
- `pico add` fetches the index (sparse, over HTTPS), resolves versions and downloads sources from git.
- The index URL can be overridden in `project.json` (`"registry": "…"`) for mirrors and private
  registries.
