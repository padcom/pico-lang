# 14. Tooling & CLI

Everything ships in a single static `pico` binary.

## 14.1 Commands
| Command | Purpose |
|---------|---------|
| `pico run file.pi [args]` | compile with the LLVM JIT and run (script mode) |
| `pico` (no args) | REPL |
| `pico build [--target t] [--release]` | ahead-of-time build to native or wasm |
| `pico test [filter]` | run `*.test.pi` |
| `pico bench [filter]` | run `bench()` cases in `*.bench.pi` / `*.test.pi` |
| `pico symbolize file` | resolve release stack traces using the debug file |
| `pico fmt [--check]` | format sources |
| `pico lint` | lints: naming, unused code, complexity, blank lines, docs (§14.7) |
| `pico init` | create `project.json` and `src/main.pi` |
| `pico add pkg[@ver]` / `pico remove pkg` | manage dependencies |
| `pico publish` | publish to the registry |
| `pico lsp` | language server (stdio), used by the VS Code extension |
| `pico doc` | generate HTML API docs from doc comments: the package's `index.pi` (`--std`: the standard and core libraries; `--out dir`, default `target/doc`; decision 294) |

## 14.2 Diagnostics
Errors are Elm/Rust-style: the error code, a snippet with the source span marked, an explanation
and a suggested fix.
```
error[E0308]: mismatched types
  ┌─ src/main.pi:4:19
  │
4 │ const m: Meters = new Feet(3)
  │          ------   ^^^^^^^^^^^ expected `Meters`, found `Feet`
  │          │
  │          expected because of this annotation
  │
  = note: `Meters` and `Feet` have the same shape, but pico types are nominal
  = help: convert explicitly: `Meters.fromFeet(new Feet(3))`
```
`pico explain E0308` prints the long-form explanation.

## 14.3 Testing
Tests live in `*.test.pi` files and use a Vitest-like API:
```ts
import { describe, test, expect } from "std/test"
import { add, load } from "./math"

// An async test is an `async function` passed by name (there are no async lambdas yet).
async function missingFilesReject(): Promise<void> {
  await expect(load("no-such-file")).rejects.toThrow("no-such-file")
}

describe("add", () => {
  test("adds numbers", () => {
    expect(add(1, 2)).toBe(3)
  })
  test("overflow panics", () => {
    expect(() => add(9223372036854775807, 1)).toPanic()
  })
  test("missing files reject", missingFilesReject)
})
```
Tests run in parallel, one isolated process per file (`pico test [paths] [-t filter] [--interp]
[--watch] [-j jobs]`; it finds `*.test.pi` files under the paths). Coverage is planned.

- Registering: `describe(name, fn)` (nesting; names show as `group > test`), `test`/`it(name,
  fn)` with a function or an `async function` (passed by name: there are no async lambdas yet),
  `skip`, `only` and `todo`, and the hooks `beforeEach`, `afterEach`, `beforeAll`, `afterAll`.
  The tests run once the file's top level is done, in order; the file prints one line per test
  and `pico-test: N passed, M failed, K skipped`, and exits with 1 when any failed. So
  `pico run x.test.pi` runs a single file too.
- `expect(v)` picks matchers by type: numbers and `int`s (`toBe`, `toEqual`, `toBeGreaterThan`,
  `toBeLessThan`, `…OrEqual`, `toBeCloseTo(x, digits)`, `toBeNaN`), strings (`toContain`,
  `toMatch(re)`, `toHaveLength`), `bool`s (`toBeTruthy`, `toBeFalsy`), arrays (`toEqual`,
  `toContain`, `toHaveLength`), functions (`toThrow(message?)`, `toPanic(message?)`), promises
  (`await expect(p).resolves.toBe(v)`, `await expect(p).rejects.toThrow(message?)`) and any
  other value (`toBe`, `toEqual`, `toBeNull`, `toBeDefined`). `.not` negates. Values compare by
  their debug form (a class by its string form), so `toBe` doesn't test identity.
- A failed expectation stops the test (in an async test, after an `await`, it's recorded and the
  test continues). A panic or an exception escaping the test fails it, and the file goes on.
- Environment: `PICO_TEST_FILTER`, `PICO_TEST_MODE=bench` and `PICO_TEST_TIMES=0` (no durations).

### Benchmarks
```ts
import { bench } from "std/test"
bench("sort 10k", () => data.slice().sort())    // warmup + adaptive iterations; mean/median/stddev
```
`pico bench [paths]` runs the benchmarks in `*.bench.pi` and `*.test.pi` files, one file at a
time and optimized: a warm-up that doubles the iterations until a batch takes 20 ms, then 20
samples of about 25 ms; it prints mean, median and standard deviation per iteration.
`pico test` and `pico run` skip benchmarks.

## 14.4 Formatter
A Prettier-like style with **a few options**, set in `project.json`:
```json
{ "fmt": { "indent": 2, "lineWidth": 100, "quotes": "double", "semicolons": false } }
```
`pico fmt --check` lists the files that aren't formatted and fails if there are any. The formatter
rewrites only the whitespace between tokens (comments are kept): indentation, spacing around
commas and brackets, blank lines and quotes; it doesn't break long lines or change semicolons
yet, and skips files with syntax errors. Decision 265.

## 14.5 Debugging
Debug builds (`pico build` without `--release`) emit DWARF (CodeView on Windows): a line table
for every pico source, the core library's too, so breakpoints by file and line and backtraces
work in lldb and gdb; on macOS the DWARF goes into `program.dSYM`. `pico dap` is a Debug Adapter Protocol server
(on top of lldb-dap) that the VS Code extension uses for breakpoints, stepping, watches and
pretty-printing of pico values (strings, arrays, maps, sets, enums): a `launch` whose `program`
is a `.pi` file builds it first, and lldb loads pico's formatters. Decisions 280–282.

## 14.6 Builds
- **Incremental**: `pico build` keeps a fingerprint of each output: the compiler, the options and
  target, and the content of every input (the entry, the modules it imports, the standard
  library modules it uses, files read by `comptime` code, `project.json`). When none changed and
  the output is still the one it built, the build does nothing (`--force` builds anyway).
  Programs that import C headers or JavaScript modules, and browser builds, always build.
  Planned: code generation per module (the core library and each module as object files of
  their own), cached by content. Decision 279.
- **Single binary**: the `pico` executable bundles LLVM, lld, libclang (for `c:` imports), the
  runtime and the stdlib sources.

## 14.7 Lint
`pico lint` checks the project's modules and reports **warnings** (as `W0xxx` diagnostics, like
the compiler's errors); it exits with status 1 when there are any. The language server shows
them in the editor too. Rules are set in `project.json`'s `"lint"`:
```json
{ "lint": { "naming": true, "unused": true, "complexity": 4, "blankLines": true, "docs": true } }
```
Each rule is off with `false`, on with its defaults with `true`, or on with options. `naming` and
`unused` are on unless turned off; the other three are off unless turned on.

| Rule | Checks |
|------|--------|
| `naming` | camelCase values and functions, PascalCase types, enum variants and type parameters (decision 39) |
| `unused` | variables (read, not only assigned), parameters after the last used one (not those an overridden or implemented method fixes), module-level bindings that aren't exported, private methods and imports that are never used. Exempt: names starting with `_`, bindings with a type test (`catch (e: E)`, `s: string =>`, `c as R`), `comptime` bindings, and the protocol methods the language calls itself (`toString`, `equals`, `hashCode`, `compareTo`, `format`, `iterator`, `next`, `dispose`, `disposeAsync`). Private fields aren't checked (printing, reflection and derived methods read them) |
| `complexity` | the cyclomatic complexity of each function: at most `true` → 4, or the number given (`{ "max": n }` too) |
| `blankLines` | a blank line before a statement `if` (not `else if`), `switch` and `match`, and before a function, method, constructor, getter or setter, unless it's the first in its block or type body; comments and decorators that belong to it go after the blank line. `{ "before": ["if", "function", …] }` picks the constructs |
| `docs` | a doc comment (`/** … */` or `///`, with a description) on every exported declaration of a module, and on the public members (methods, getters, setters, fields) of exported types; a member that implements or overrides a documented member is documented by it |

**Complexity** starts at 1 per function and adds 1 for each `if` and `else if`, loop (`for`,
`for-of`, `while`, `do`, `loop`), `case` and `match` arm, `catch`, conditional expression, and
each `&&`, `||` and `??`. A closure counts as a function of its own and adds nothing to the
function around it.

`if` and `match` in expression position (`const x = if (c) 1 else 2`) don't need a blank line.
`pico fmt` doesn't insert the blank lines. Decision 277.
