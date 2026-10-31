# 14. Tooling & CLI

Everything ships in a single static `ppl` binary.

## 14.1 Commands
| Command | Purpose |
|---------|---------|
| `ppl run file.ppl [args]` | compile with the LLVM JIT and run (script mode) |
| `ppl` (no args) | REPL |
| `ppl build [--target t] [--release]` | ahead-of-time build to native or wasm |
| `ppl test [filter]` | run `*.test.ppl` |
| `ppl fmt [--check]` | format sources |
| `ppl lint` | lints (naming conventions etc.) |
| `ppl init` | create `project.json` and `src/main.ppl` |
| `ppl add pkg[@ver]` / `ppl remove pkg` | manage dependencies |
| `ppl publish` | publish to the registry |
| `ppl lsp` | language server (stdio), used by the VS Code extension |
| `ppl doc` | generate API docs from `/** */` comments |

## 14.2 Diagnostics
Errors are Elm/Rust-style: the error code, a snippet with the source span marked, an explanation
and a suggested fix.
```
error[E0308]: mismatched types
  ┌─ src/main.ppl:4:20
  │
4 │ const m: Meters = new Feet(3)
  │          ------   ^^^^^^^^^^^ expected `Meters`, found `Feet`
  │          │
  │          expected because of this annotation
  │
  = note: `Meters` and `Feet` have the same shape, but ppl types are nominal
  = help: convert explicitly: `Meters.fromFeet(new Feet(3))`
```
`ppl explain E0308` prints the long-form explanation.

## 14.3 Testing
Tests live in `*.test.ppl` files and use a Vitest-like API:
```ts
import { describe, test, expect, beforeEach } from "std/test"
import { add } from "./math"

describe("add", () => {
  test("adds numbers", () => {
    expect(add(1, 2)).toBe(3)
  })
  test("overflow panics", () => {
    expect(() => add(i64.MAX, 1)).toPanic()
  })
  test("async works", async () => {
    await expect(load("x")).rejects.toThrow(IoError)
  })
})
```
Tests run in parallel, one isolated worker per file. `--watch` and coverage are supported.

## 14.4 Formatter
A Prettier-like style with **a few options**, set in `project.json`:
```json
{ "fmt": { "indent": 2, "lineWidth": 100, "quotes": "double", "semicolons": false } }
```

## 14.5 Debugging
Debug builds emit DWARF (CodeView on Windows). `ppl dap` is a Debug Adapter Protocol server
(on top of lldb) that the VS Code extension uses for breakpoints, stepping, watches and
pretty-printing of ppl values (strings, arrays, maps, enums).

## 14.6 Builds
- **Incremental**: per-module caching of parsed, type-checked and lowered IR, keyed by content
  hash. Monomorphized instances are cached per instantiation.
- **Single binary**: the `ppl` executable bundles LLVM, lld, libclang (for `c:` imports), the
  runtime and the stdlib sources.

## Open questions
- Is the `semicolons` formatter option worth it, or always none?
- Benchmarks (`bench()` in std/test)?
