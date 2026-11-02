# 14. Tooling & CLI

Everything ships in a single static `ppl` binary.

## 14.1 Commands
| Command | Purpose |
|---------|---------|
| `ppl run file.ppl [args]` | compile with the LLVM JIT and run (script mode) |
| `ppl` (no args) | REPL |
| `ppl build [--target t] [--release]` | ahead-of-time build to native or wasm |
| `ppl test [filter]` | run `*.test.ppl` |
| `ppl bench [filter]` | run `bench()` cases in `*.bench.ppl` / `*.test.ppl` |
| `ppl symbolize file` | resolve release stack traces using the debug file |
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
  ┌─ src/main.ppl:4:19
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
Tests run in parallel, one isolated process per file (`ppl test [paths] [-t filter] [--interp]
[--watch] [-j jobs]`; it finds `*.test.ppl` files under the paths). Coverage is planned.

- Registering: `describe(name, fn)` (nesting; names show as `group > test`), `test`/`it(name,
  fn)` with a function or an `async function` (passed by name: there are no async lambdas yet),
  `skip`, `only` and `todo`, and the hooks `beforeEach`, `afterEach`, `beforeAll`, `afterAll`.
  The tests run once the file's top level is done, in order; the file prints one line per test
  and `ppl-test: N passed, M failed, K skipped`, and exits with 1 when any failed. So
  `ppl run x.test.ppl` runs a single file too.
- `expect(v)` picks matchers by type: numbers and `int`s (`toBe`, `toEqual`, `toBeGreaterThan`,
  `toBeLessThan`, `…OrEqual`, `toBeCloseTo(x, digits)`, `toBeNaN`), strings (`toContain`,
  `toMatch(re)`, `toHaveLength`), `bool`s (`toBeTruthy`, `toBeFalsy`), arrays (`toEqual`,
  `toContain`, `toHaveLength`), functions (`toThrow(message?)`, `toPanic(message?)`), promises
  (`await expect(p).resolves.toBe(v)`, `await expect(p).rejects.toThrow(message?)`) and any
  other value (`toBe`, `toEqual`, `toBeNull`, `toBeDefined`). `.not` negates. Values compare by
  their debug form (a class by its string form), so `toBe` doesn't test identity.
- A failed expectation stops the test (in an async test, after an `await`, it's recorded and the
  test continues). A panic or an exception escaping the test fails it, and the file goes on.
- Environment: `PPL_TEST_FILTER`, `PPL_TEST_MODE=bench` and `PPL_TEST_TIMES=0` (no durations).

### Benchmarks
```ts
import { bench } from "std/test"
bench("sort 10k", () => data.slice().sort())    // warmup + adaptive iterations; mean/median/stddev
```
`ppl bench [paths]` runs the benchmarks in `*.bench.ppl` and `*.test.ppl` files, one file at a
time and optimized: a warm-up that doubles the iterations until a batch takes 20 ms, then 20
samples of about 25 ms; it prints mean, median and standard deviation per iteration.
`ppl test` and `ppl run` skip benchmarks.

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
