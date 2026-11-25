# 14. Tooling & CLI

Everything ships in a single static `pico` binary.

## 14.1 Commands
| Command | Purpose |
|---------|---------|
| `pico run file.pi [args]` | compile with the LLVM JIT and run (script mode) |
| `pico` (no args) | REPL |
| `pico build [--target t] [--release] [-L dir]` | ahead-of-time build to native or wasm (`-L`: where `@link` libraries are, §12.1) |
| `pico test [filter]` | run `*.test.pi` |
| `pico bench [filter]` | run `bench()` cases in `*.bench.pi` / `*.test.pi` |
| `pico symbolize file` | resolve release stack traces using the debug file |
| `pico fmt [--check]` | format sources |
| `pico lint` | lints: naming, unused code, complexity, blank lines, docs (§14.7) |
| `pico init` | create `project.json` and `src/main.pi` |
| `pico add pkg[@ver]` / `pico remove pkg` | manage dependencies |
| `pico publish` | publish to the registry |
| `pico lsp` | language server (stdio), used by the VS Code extension |
| `pico doc` | generate HTML API docs from doc comments: the package's `index.pi` (`--std`: the standard and core libraries; `--out dir`, default `target/doc`; decision 294); `@example` blocks are shown after the description, and `--json file` writes the reference as data instead (decision 348) |

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

// An async test is an `async function`, or an async lambda: `test("…", async () => { … })`.
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
[--watch] [-j jobs]`; it finds `*.test.pi` files under the paths; `-j` defaults to one file per
CPU, but no more than the available memory holds at about 512 MB each). Coverage is planned.

- Registering: `describe(name, fn)` (nesting; names show as `group > test`), `test`/`it(name,
  fn)` with a function, an `async function` or an async lambda (`async () => { … }`),
  `skip`, `only` and `todo`, and the hooks `beforeEach`, `afterEach`, `beforeAll`, `afterAll` (each also taking an async
  function, which the runner awaits).
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
- **License notices**: `pico build` writes `<output>.notices.txt` next to the executable
  (`app.notices.txt`, `app.exe.notices.txt`), the license texts to ship with it: pico's own
  (the runtime and the standard library, MIT) and those of the third-party code linked in, as
  the linker's map shows it. Mbed TLS (Apache-2.0) when the program uses TLS or `std/crypto`;
  compiler-rt's builtins when any are needed; musl (MIT) on musl targets and for Windows' math;
  mingw-w64's CRT and winpthreads on Windows; wasi-libc and wasmtime's preview 1 adapter on
  `wasm32-wasi`. Programs for glibc and macOS link the system's C library dynamically, so it
  isn't listed. The release archives carry the vendored sources' license files under
  `licenses/`.
- **Single binary**: the `pico` executable bundles LLVM, lld, libclang (for `c:` imports), the
  runtime and the stdlib sources.
- **Timings**: with `PICO_TIMINGS=1` in the environment, a check prints how long each stage
  took (parse, imports, decorators, resolve, typecheck, mir, comptime) and the compiler's peak
  memory so far, and `pico build`/`pico run` print the peak once the build is done, code
  generation and linking included (on Unix; the program's own memory isn't counted).

## 14.7 Lint
`pico lint` checks the project's modules and reports **warnings** (as `W0xxx` diagnostics, like
the compiler's errors); it exits with status 1 when there are any. The language server shows
them in the editor too. A file's own errors are reported with its warnings: those of parsing,
name resolution, type checking and the C and JavaScript signatures. `pico lint` stops there, before
lowering, so errors only a build finds (a constant expression that always panics, failing
`comptime` code) are `pico build`'s. Files are linted in parallel: `-j N` at a time, by default
one per CPU but no more than the available memory holds (about 256 MB each, #166). Rules are set in
`project.json`'s `"lint"`:
```json
{
  "lint": {
    "complexity": 4,
    "blankLines": true,
    "docs": true,
    "maxParams": 4,
    "quotes": "single",
    "overrides": [{ "files": "*.test.pi", "rules": { "maxNestedCallbacks": 4 } }]
  }
}
```
Each rule is off with `false`, on with its defaults with `true`, or on with options (a maximum as
a number or `{ "max": n }`; `quotes` as `"single"` or `"double"`). `naming`, `unused` and
`explicitReturn` are on unless turned off; the others are off unless turned on. An unknown rule
is reported with the closest rule's name.

**Overrides.** `"overrides"` lists `{ "files": pattern or [patterns], "rules": { … } }`; for a
file that matches, its rules are applied on top of the others, in order. Patterns are relative
to the `project.json`'s directory: `*` matches within a path segment, `**` across segments, `?`
one character, and a pattern without `/` matches the file name in any directory.

| Rule | Checks |
|------|--------|
| `naming` | camelCase values and functions, PascalCase types, enum variants and type parameters (decision 39); constants, static fields and static getters may be UPPER_SNAKE_CASE; a leading `_` and a trailing one (`async_`, avoiding a keyword) are ignored, and `__` names (the core library's internals) aren't checked |
| `unused` | variables (read, not only assigned), parameters after the last used one (not those an overridden or implemented method fixes), module-level bindings that aren't exported, private methods and imports that are never used. Exempt: names starting with `_`, bindings with a type test (`catch (e: E)`, `s: string =>`, `c as R`), `comptime` bindings, and the protocol methods the language calls itself (`toString`, `equals`, `hashCode`, `compareTo`, `format`, `iterator`, `next`, `dispose`, `disposeAsync`). Private fields aren't checked (printing, reflection and derived methods read them) |
| `complexity` | the cyclomatic complexity of each function: at most `true` → 4, or the number given (`{ "max": n }` too) |
| `blankLines` | a blank line before a statement `if` (not `else if`), `switch`, `match`, `return`, loop (`for`, `while`, `do`, `loop`), `try`, block statement, `export`, and before a function, method, constructor, getter, setter or class, unless it's the first in its block or type body; one after the imports (`import`), and one between class members after a multi-line member (`member`). Loops, `try` and blocks don't need one after a `const`/`let` or an `if`. Comments and decorators that belong to it go after the blank line. `{ "before": ["if", "function", …] }` picks the constructs |
| `docs` | a doc comment (`/** … */` or `///`, with a description) on every exported declaration of a module, and on the public members (methods, getters, setters, fields) of exported types; a member that implements or overrides a documented member is documented by it |
| `explicitReturn` | a function whose result is a simple value returns it with `return`: a variable, a literal (number, boolean, `null`, string, template, array, object), an operation (`a + b`, `!x`, `a ?? b`, `c ? a : b`, `x as T`, `x!`, a range), a member or element read (`return sum`, not a final `sum`); calls, `new`, `if`, `match`, `switch`, blocks, `try`, `await` and closures stay implicit; expression-bodied arrows, constructors, setters, generators, `: void` functions and blocks used as values are exempt (decision 331) |

The rules from the author's ESLint configuration (decision 332), each off unless turned on;
`pico explain W0xxx` shows an example of each:

| Rule | Checks |
|------|--------|
| `maxParams` | a function's parameters: at most 4 (`extern` declarations aren't checked) |
| `maxDepth` | `if` (an `else if` is part of its `if`), loops, `switch`, `match` and `try` nested in one function: at most 2; a closure starts again at 0 |
| `maxLines` | a function's lines, blank lines and comments included: at most 32 |
| `maxNestedCallbacks` | functions passed as arguments, nested inside each other: at most 2 |
| `maxStatementsPerLine` | statements starting on one line: at most 2 |
| `maxLen` | characters on a line: at most 130 |
| `shadow` | a declaration that hides a variable, parameter, function, type or import of an enclosing scope (not class members) |
| `unusedExpressions` | a dropped value that does nothing: a variable, literal, operation, member read or conditional expression, or an object created with `new` and dropped, as a statement that isn't its block's value (a `: void` function's last statement is dropped too) |
| `unreachable` | statements after `return`, `throw`, `break` or `continue` in the same block (function declarations aside) |
| `selfCompare`, `selfAssign` | `x == x` (and the other comparisons; not on floats, where `x != x` tests for NaN), `x = x`, for variables and member chains |
| `constantBinary` | `??` and `??=` whose left side's type can't be `null` |
| `unsafeFinally` | `return`, `throw`, and `break`/`continue` of a loop outside it, in a `finally` block |
| `requireYield` | a generator with a body but no `yield` |
| `forDirection` | a `for (…; i < n; i--)` whose update moves the counter away from the condition's limit |
| `emptyBlock` | an empty block, `catch` or `switch` (not a function body; a comment inside makes it non-empty) |
| `dupeElseIf` | an `else if` condition that an earlier one in the chain (or one side of its `\|\|`) already tested |
| `uselessCatch` | a last `catch` whose body only rethrows what it caught |
| `templateInString` | a plain string containing `${…}` |
| `loopFunc` | a closure in a loop that captures a `let` declared outside the loop and changed somewhere |
| `floatingPromise` | a dropped `Promise` that can fail (its error type isn't `never`; a statement that isn't its block's value), unless it ends with `.catch(…)` or `.then(onValue, onError)` |
| `awaitInLoop` | `await` in a loop (not in a `for await` loop's body) |
| `requireAwait` | an `async` function with a body but no `await` or `for await` (async generators aside) |
| `preferAwait` | `.then`/`.catch`/`.finally` on a promise, a `callback`/`cb` parameter, a callback argument whose first parameter is `err`/`error` |
| `promiseReturnWrap` | `Promise.resolve(…)`/`Promise.reject(…)` as a promise callback's result |
| `promiseParamNames` | `new Promise`'s executor parameters not named `resolve`, `reject` (optionally with a leading `_`) |
| `preferConst` | a `let` with an initializer (or a `for (let x of …)`) none of whose names is ever reassigned |
| `objectShorthand` | `{ x: x }` in an object or struct literal |
| `uselessRename` | `{ x: x }` in a pattern, `import { a as a }`, `export { a as a }` |
| `operatorAssignment` | `x = x op y` for arithmetic and bitwise operators |
| `preferTemplate` | a `+` chain mixing string literals and other values |
| `uselessConcat` | two string literals joined with `+` on one line |
| `arrowBodyStyle` | an arrow function whose block only holds `return value` |
| `preferArrowCallback` | a `function` expression passed as an argument (unless it's a generator or uses `this` or its own name) |
| `funcStyle` | a `function` expression assigned to a variable (arrow functions are fine) |
| `lonelyIf` | an `else` block holding only an `if` |
| `loneBlocks` | a block statement that declares nothing |
| `uselessReturn` | a bare `return` as a function's last statement |
| `uselessConstructor` | a class's only constructor, public, without parameter properties, defaults or decorators, that is empty (no base class) or only calls `super` with its parameters |
| `preferDestructuring` | `const x = o.x` where `o` is a struct or class instance and `x` a field (not a getter) |
| `preferExponentiation` | `Math.pow(a, b)` |
| `yoda` | a comparison with the literal first (`5 < n`) |
| `groupedAccessors` | a setter not right after the getter of the same property |
| `curly` | a body without braces that doesn't end on the line of the token before it |
| `extraParens` | parentheses around a variable, literal, call, member read or parenthesized expression, and around a whole initializer, argument, element, field value, template part, returned/thrown/awaited value, condition or statement (not around an object literal or block) |
| `mixedOperators` | operators of one family (arithmetic, bitwise, comparison, logical) with different precedence mixed without parentheses |
| `commaDangle` | a missing trailing comma in a multi-line array, object, argument list, parameter list, pattern or import/export list (its closing bracket on a line of its own), or one in a one-line list; never after a rest element |
| `braceStyle` | `{` not on the line of what it opens, `}` and the following `else`/`catch`/`finally` on different lines, statements on the `{` or `}` line of a multi-line block |
| `operatorLinebreak` | a line ending with a binary operator, `=` or `?`/`:` where the next line continues the expression; `+` and `-` stay at the end (a line starting with them is a new statement, §1.2) |
| `multilineTernary` | a conditional expression over several lines whose condition and branches don't each have a line |
| `spacedComment` | no space after `//`, `///`, `/*` or `/**` (`/*-` allowed) |
| `extraSemi` | a `;` at the end of a line or an empty statement; kept: one ending a block's last expression (it makes the block `void`), separators on one line, `for` headers, and one before a line starting with `(`, `[`, a template, `+`, `-` or `/` |
| `quotes` | strings in the other kind of quotes (`"single"` by default, or `"double"`), unless that avoids an escape; with `"double"`, one-character `'x'` (maybe a `char`) is left alone |
| `docTags` | in a function's doc comment: `@param` for each parameter, in order, named right, once, with a description; `@returns` with a description exactly when the declared result isn't `void`, `never` or `Promise<void>`; `@yields` exactly on generators; no `{types}` or `[defaults]`; known tag names; no text on tags that take none; valid `@access` and `@version`/`@since`; `@implements` only on classes. `@inheritdoc` exempts the comment |
| `docFormat` | in `/** … */`: `*` aligned under the opening `*`, one `*` per line, no text on the first or last line of a multi-line block, one space after `*` outside `@example`; in every doc comment, one blank line between description and tags and none between tags |
| `underscoreDangle` | a variable, parameter or function whose name starts or ends with `_` but is used; unused ones are fine, that's how to keep state or parameters you don't need. `__` names (the core library's internals) aren't reported |
| `preferNamedCaptureGroup` | a regex with an unnamed capture group (`(…)`, not `(?:…)` or `(?<name>…)`) |

Not taken over: `no-return-await` (pico doesn't flatten a returned promise, so `return await p`
is needed), the rules about JavaScript features pico doesn't have, and the ones the compiler
enforces (decision 331).

**Complexity** starts at 1 per function and adds 1 for each `if` and `else if`, loop (`for`,
`for-of`, `while`, `do`, `loop`), `case` and `match` arm, `catch`, conditional expression, and
each `&&`, `||` and `??`. A closure counts as a function of its own and adds nothing to the
function around it.

`if` and `match` in expression position (`const x = if (c) 1 else 2`) don't need a blank line.
`pico fmt` doesn't insert the blank lines. The standard library follows the rules that are on by default (a test lints it). Decisions 277, 331, 332, 333.
