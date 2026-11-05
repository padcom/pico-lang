# Implementation plan

This plan breaks the pico compiler into **small tasks**, each sized to fit in one working session.
Work through them in order. Live status is tracked in [PROGRESS.md](PROGRESS.md), the language
definition is in [docs/spec/](docs/spec/), and the reasoning behind each design choice is in
[docs/DECISIONS.md](docs/DECISIONS.md).

## How tasks are written
Every task has:
- **ID**: `P<phase>.<n>`, used in PROGRESS.md and commit messages (`P2.3: …`).
- **Goal**: one sentence.
- **Spec**: the sections to read first. Read *only* these, to save tokens.
- **Do**: the concrete deliverables.
- **Done when**: the tests or commands that prove the task is finished.

Ground rules that apply to every task:
1. Before starting, read PROGRESS.md and the task's spec sections. Read nothing else unless you need it.
2. Every feature comes with golden tests (`tests/run`, `tests/fail`) or Rust unit tests.
3. `scripts/test.sh` has to pass before a commit, and you commit right after it does (no need to ask).
4. Update PROGRESS.md: tick the task, write a 1–3 line log entry, and set **Next task**.
5. Keep tasks small. If one turns out bigger than expected, split it into `P2.3a`/`P2.3b` here.
6. Spec gaps found while implementing get a decision, which is recorded in DECISIONS.md and the spec chapter.

---

## Architecture (bootstrap compiler)

```
 .pi ──► pico-syntax ──► pico-sema ──────► pico-mir ─────► pico-codegen ──► .o ──► linker ──► exe/wasm
          lexer          resolve names    monomorphize   LLVM (inkwell)          (cc now,
          parser → AST   type check       lower → CFG    debug info              bundled lld later)
                         → typed HIR      pico-interp ◄── used for comptime and `--interp` tests
```

| Crate | Responsibility |
|-------|----------------|
| `pico-diag` | source map, spans, diagnostics rendering (Elm/Rust style), error codes |
| `pico-syntax` | tokens, lexer, newline rule, AST, parser, AST pretty-printer |
| `pico-sema` | name resolution, types, inference, narrowing, checked throws, typed HIR |
| `pico-mir` | mid-level IR (CFG of basic blocks), lowering from HIR, monomorphization, MIR dump |
| `pico-interp` | MIR interpreter: comptime, `pico run --interp`, and a second test oracle |
| `pico-codegen` | MIR → LLVM IR via inkwell, object emission, target machine setup |
| `pico-runtime` | runtime in **C** (strings, alloc/GC, panics, unwinding, event loop), compiled per target with clang |
| `pico-driver` | the pipeline, caching, target selection, linking |
| `pico-cli` | the `pico` binary (subcommands) |
| `pico-tests` | golden-test harness |

**Why the runtime is in C:** it's compiled with clang for every target (we bundle the same LLVM),
it's easy to link into native and wasm, and it's simple to swap pieces for pico code later.

**Golden tests:**
- `tests/run/**/*.pi` hold `// expect:` lines (stdout), plus optional `// exit: N` and `// panic: msg`.
  Each test runs through the interpreter and as a native binary (also with `PICO_GC_STRESS=1`,
  from P4.2). All outputs must match.
- `tests/fail/**/*.pi` hold `// error: E0xxx` and `// at: line:col`. Compilation must fail with exactly
  those errors.
- Diagnostics rendering is checked with `insta` snapshots.

**Risks to keep in mind:**
- **LLVM can't emit WasmGC.** `wasm32-browser` therefore starts on linear memory with the pico GC
  (the same as WASI). A WasmGC backend is a separate, later emitter (P8.7).
- Unwinding: native targets use LLVM Itanium/SEH exception handling and wasm uses the wasm
  exception-handling proposal. If that blocks progress, fall back to explicit error-return lowering
  (P3.19 decides).
- Precise GC needs root maps. Start with a **shadow stack**, which is portable and works on wasm,
  and optimize later.

---

## Phase 1: Foundation (front end, no codegen)

### P1.1 Toolchain setup
- **Goal**: get a reproducible development environment.
- **Do**: install rustup (stable) and LLVM via Homebrew (pin a version inkwell supports: LLVM 23 with inkwell 0.11, see docs/DEVELOPMENT.md). Write
  `docs/DEVELOPMENT.md` with the exact versions, `LLVM_SYS_*_PREFIX` setup and the commands. Add
  `rust-toolchain.toml`.
- **Done when**: `cargo --version`, `llvm-config --version` and `clang --version` work and are documented.

### P1.2 Workspace skeleton
- **Goal**: an empty Cargo workspace with all the crates, plus the test script.
- **Do**: create the crates from the table (empty libs), `pico-cli` with `pico --version`, and
  `scripts/test.sh` (fmt check, clippy, `cargo test`). Add a `.gitignore` for `target/`.
- **Done when**: `scripts/test.sh` passes and `cargo run -p pico-cli -- --version` prints the version.

### P1.3 Diagnostics
- **Spec**: 14 §14.2
- **Do**: `SourceMap`, `FileId`, `Span`, `Diagnostic { code, severity, message, labels, notes, help }`
  and a renderer that matches the spec's example (custom, or `ariadne`/`codespan-reporting`).
- **Done when**: an insta snapshot of a rendered diagnostic matches §14.2's layout.

### P1.4 Lexer: core tokens
- **Spec**: 01 §1.1, §1.3 (except templates), §1.5, §1.6
- **Do**: the `Token` enum, keywords, identifiers, punctuators (longest match), integer and float
  literals (hex, octal, binary, `_`, suffixes `u8`…`f64`), `'…'`/`"…"` with escapes, comments, and
  newlines kept as tokens.
- **Done when**: unit tests cover every literal form and the error cases (unterminated string, bad
  suffix).

### P1.5 Lexer: templates, multiline strings, regex
- **Spec**: 01 §1.3, §1.4
- **Do**: backtick templates with nested `${}` (a mode stack), format-spec capture after a top-level `:`,
  triple-quoted `'''`/`"""`/```` ``` ```` with indent stripping, and regex literals using the JS rule
  (the lexer is told by the previous token whether an expression can start).
- **Done when**: unit tests for nesting (`` `a${`b${c}`}` ``), `${x:.2f}`, `${a ? b : c}` (no spec),
  dedent behavior, `a / b / c` vs `/re/g`.

### P1.6 Statement-termination rule
- **Spec**: 01 §1.2
- **Do**: a token filter that turns newlines into statement terminators or drops them, using the
  bracket depth, the operator at the end of the line and the operator at the start of the next line.
- **Done when**: table-driven tests cover every example in §1.2, including `f\n(x)` as two statements.

### P1.7 AST and pretty-printer
- **Spec**: skim 03, 04, 05
- **Do**: AST node types for expressions, statements, declarations and types (cover the *whole*
  language now, even though the parser fills it in gradually). Add a `pico parse --dump file.pi`
  S-expression printer.
- **Done when**: it compiles and the printer has unit tests on hand-built ASTs.

### P1.8 Parser: expressions
- **Spec**: 03 §3.3, §3.6
- **Do**: a Pratt parser using the precedence table, covering literals, identifiers, unary and binary
  operators, assignment, ternary, calls (positional + named arguments), member access, `?.`,
  indexing, `!`, `as`, ranges `a..b`, and arrow functions.
- **Done when**: snapshot tests of `--dump` output for precedence and associativity cases.

### P1.9 Parser: statements and blocks
- **Spec**: 03 §3.1–3.2, 04 §4.1, §4.3
- **Do**: `const`/`let`, block expressions, `if`/`else` as an expression, `while`, `do-while`, C-style
  `for`, `for-of`, `loop`, `break [value]`, `continue`, `return`, and the block-vs-object-literal rule.
- **Done when**: snapshot tests pass, including `{` at statement start parsing as a block.

### P1.10 Parser: declarations and types
- **Spec**: 02 (type syntax), 05 §5.1–5.3, 06 §6.1–6.2, 04 §4.4
- **Do**: functions (params, defaults, `?`, rest, return type, `throws`), type syntax (unions, `T?`,
  arrays, `[T; N]`, tuples, generics, function types, `immutable`), `struct`, `enum`, `class`,
  `interface`, `type`, `distinct`, `import`/`export`, decorators `@x(...)`, modifiers.
- **Done when**: every code sample in spec chapters 00–06 parses (add them as test fixtures).

### P1.11 Parser: patterns, match, switch, try
- **Spec**: 03 §3.5, 07 §7.3
- **Do**: the full pattern grammar, `match`, `switch`/`case`/`default`, `is`, `try`/`catch`/`finally`
  as an expression, `try?`/`try!`, `throw`, and `using`/`await using`.
- **Done when**: every code sample in spec chapters 03 and 07 parses.

### P1.12 Parser error recovery
- **Do**: sync points (statement start, `}`), "expected X, found Y" diagnostics with helpful
  suggestions, and several errors per file.
- **Done when**: `tests/fail/parse/*.pi` show sensible multi-error output (snapshots).

### P1.13 Golden-test harness
- **Do**: the `pico-tests` crate that discovers `tests/run` and `tests/fail` and parses the `// expect:`,
  `// error:`, `// exit:` and `// panic:` directives. For now it runs the stages that exist (parse only)
  and reports clear diffs.
- **Done when**: `cargo test -p pico-tests` runs the fixtures. One deliberately broken fixture shows a
  readable failure, and is then fixed.

---

## Phase 2: First executable (hello world, native and interpreted)

### P2.1 Name resolution
- **Spec**: 04 §4.1–4.2, 08 §8.1 (single file for now)
- **Do**: scopes, block scoping, use-before-declaration errors, order-independent top-level
  `function`/type declarations, entry-module script body, and globals (`println` etc.) as built-ins.
- **Done when**: fail tests cover unknown names, use-before-declaration and duplicate declarations.

### P2.2 Type checker: primitives and functions
- **Spec**: 02 §2.1–2.2, 03 §3.1, 05 §5.1
- **Do**: the type representation, a primitive type table, literal typing from context (default
  `int`/`number`), lossless-widening rules, `as`, operator typing, blocks/`if` as expressions,
  `void`/`never`, function signatures, and return-type inference (required for recursive functions).
- **Done when**: run tests type-check and fail tests cover mismatches, narrowing without `as`, and
  `if` without `else` (typed `T?`; the null runtime comes in P3.6).

### P2.3 MIR design and lowering
- **Do**: the MIR (functions → basic blocks → statements over typed locals; terminators: goto, branch,
  return, call, panic), lowering of the P2.2 subset, and `pico build --emit mir`.
- **Done when**: MIR dump snapshots exist for loops, `if` expressions and `loop`/`break value`.

### P2.4 MIR interpreter
- **Do**: `pico-interp` executes MIR. Integers trap on overflow, and `println` prints primitives and
  string literals. Add `pico run --interp`.
- **Done when**: the harness runs `tests/run` in interpreter mode and hello world passes.

### P2.5 C runtime v0
- **Spec**: 07 §7.5
- **Do**: `runtime/` C sources: `pico_println_*`, `pico_panic(msg, file, line)` (prints and exits 101),
  `main` calling `pico_entry`, overflow/bounds panic helpers, and a build via the `cc` crate for the host.
- **Done when**: a C unit test (or Rust test linking the runtime) shows the panic output format.

### P2.6 LLVM codegen v0
- **Do**: inkwell setup, function and local lowering, integer/float/bool arithmetic using
  `llvm.*.with.overflow` and a panic on overflow, comparisons, branches, loops, calls, and runtime
  calls for printing. Emit `.o` for the host target.
- **Done when**: `--emit llvm-ir` snapshots for simple functions match.

### P2.7 `pico build` / `pico run` end to end
- **Do**: the driver links object files with the system `cc` into an executable. `pico run` builds to
  a cache directory and executes the result. The harness runs every `tests/run` test natively too and
  compares outputs.
- **Done when**: `pico run hello.pi` prints `Hello, world!`, and the interpreter and native outputs
  agree on all tests (fib, loops, overflow panic). **Milestone M1.**

---

## Phase 3: Core language (single module, leaking allocator)

### P3.1 Allocator v0 and string runtime
- **Spec**: 02 §2.1 (string), 09 §9.1
- **Do**: the `pico_alloc` API (malloc-backed, nothing freed yet, but behind the API the GC will use)
  and a UTF-8 string object (length in code points cached, plus byte length). Concatenation, `==`,
  `.length`, and `s[i]` (O(n) with a cached index) returning `char`.
- **Done when**: run tests cover string ops and multibyte text (emoji, Polish diacritics).

### P3.2 char type and template literals
- **Spec**: 01 §1.3 (char inference), §1.4
- **Do**: `char` inference from one-character literals, template literal lowering (`${}` without
  specs), multiline literals, and a `toString` protocol for primitives.
- **Done when**: run tests pass, and a fail test covers `const c: char = "ab"`.

### P3.3 Format specs
- **Spec**: 01 §1.4
- **Do**: parse the format-spec mini-language at compile time (invalid spec → compile error), with
  runtime formatting functions for ints, floats and strings (fill/align/sign/#/0/width/precision/type),
  plus `?` debug output for primitives.
- **Done when**: every example in the §1.4 table is a run test.

### P3.4 Structs
- **Spec**: 02 §2.7, 03 §3.2
- **Do**: struct declarations, both literal forms, field access and assignment, methods with
  implicit members, value-copy semantics, field-wise `==`, and nested structs.
- **Done when**: run tests show copy semantics (mutating a copy doesn't affect the original).

### P3.5 Arrays, fixed arrays, tuples, ranges
- **Spec**: 02 §2.8, 04 §4.3, 01 (ranges)
- **Do**: `T[]` (growable: push, pop, length, index with a bounds panic), `[T; N]` inline, tuples
  `[A, B]` (accessed by destructuring or `t[0]` with a constant index), `a..b`, `.step(n)`, `for-of`
  over arrays and ranges.
- **Done when**: run tests pass, including an out-of-bounds panic test.

### P3.6 Null and narrowing infrastructure
- **Spec**: 02 §2.4
- **Do**: `T?` representation (tagged for values, null pointer for references), `null`, `?.`, `??`,
  `!` (panics), `??=`, and the flow-sensitive narrowing framework (`!= null`, early return).
- **Done when**: run and fail tests cover using `T?` as `T` without narrowing.

### P3.7 Enums and basic match
- **Spec**: 02 §2.6, 03 §3.5
- **Do**: payload enums (tagged-union layout), construction with named args, `match` with literal,
  wildcard, binding and variant (positional and named) patterns, an exhaustiveness checker
  (Maranget-style usefulness), and an error that lists the missing patterns.
- **Done when**: run tests pass, and a fail test shows the missing-case diagnostic.

### P3.8 Advanced patterns and switch
- **Spec**: 03 §3.5
- **Do**: guards, or-patterns, range patterns, `as` bindings, tuple/array patterns with `...rest`,
  type-test patterns, `is` with bindings and narrowing, and the modernized `switch` (`case a | b`).
- **Done when**: every §3.5 example is a run test.

### P3.9 Classes I
- **Spec**: 06 §6.1 (no inheritance)
- **Do**: heap layout, `new`, constructors, parameter properties, private-by-default visibility,
  methods, implicit members, get/set, static members and static init blocks, identity `==`, and
  definite assignment.
- **Done when**: run tests pass, and fail tests cover private access and an unassigned field.

### P3.10 Classes II: inheritance
- **Spec**: 06 §6.1, §6.3
- **Do**: `extends`, `super(...)`, vtables, `virtual`/`override`/`abstract`/`final`, the required
  `override` diagnostic, constructor overloading and `this(...)` delegation, and `instanceof`.
- **Done when**: run tests cover polymorphic dispatch, and fail tests cover a missing `override` and
  overriding a non-virtual method.

### P3.11 Interfaces
- **Spec**: 06 §6.2–6.3
- **Do**: nominal `implements`, itables with fat pointers, default methods, conflicting-default
  errors, structs/enums implementing interfaces (boxing), and interface inheritance.
- **Done when**: run tests pass, and a fail test covers a structurally matching class without
  `implements`.

### P3.12 Unions and literal types
- **Spec**: 02 §2.5–2.6
- **Do**: a union representation (a tag plus payload, reusing the enum machinery), literal types,
  `typeof`/`instanceof`/discriminant narrowing, and lowering discriminated object unions to
  anonymous enums.
- **Done when**: every §2.5 and §2.6 example is a run test.

### P3.13 Generics I: functions
- **Spec**: 02 §2.9
- **Do**: generic functions, type-argument inference, monomorphization in MIR (an instance cache and
  name mangling), and `extends` constraints.
- **Done when**: a run test with `max<T>` over several types passes and the MIR dump shows the instances.

### P3.14 Generics II: types and where
- **Do**: generic structs, classes and interfaces, `where` clauses, generic methods, and default
  type parameters.
- **Done when**: a user-defined generic `Stack<T>` and `Pair<A, B>` work in run tests.

### P3.15 Closures
- **Spec**: 05 §5.5–5.6
- **Do**: arrow and function expressions, closure conversion (`const` captured by value, captured
  `let` boxed), lexical `this` in arrows, and function types.
- **Done when**: the §5.5 counter example and a list of closures capturing a loop variable both work.

### P3.16 Function features
- **Spec**: 05 §5.2–5.4, §5.4.1
- **Do**: named arguments, defaults (which can reference earlier parameters), optional parameters,
  rest/spread, tuple spread, overload resolution (most specific wins, ambiguity is an error), and
  `tailcall` (`musttail`, or a compile error).
- **Done when**: run tests pass, a fail test covers an ambiguous overload, and a deep `tailcall`
  recursion doesn't overflow the stack.

### P3.17 Operators and equality protocols
- **Spec**: 03 §3.3–3.4, 02 §2.7 (protocols)
- **Do**: `operator` declarations (binary, unary, `[]`, `[]=`), derived compound assignment,
  `Equatable`/`Hashable`/`Comparable` with automatic derivation for structs, enums and tuples, and
  a built-in `@derive(Eq, Hash, Ord)` for classes (reimplemented as a pico decorator in P6.6).
- **Done when**: `Vec2` arithmetic and sorting structs by `compareTo` work.

### P3.18 Distinct types, aliases, immutability
- **Spec**: 02 §2.10–2.11
- **Do**: `type` aliases, `distinct` types, `readonly` fields, `immutable T` deep checking, and
  `readonly` methods.
- **Done when**: fail tests cover mutation through `immutable` and mixing `Meters`/`Feet`.

### P3.19 Exceptions I
- **Spec**: 07 §7.1–7.3
- **Do**: decide on and implement the unwinding strategy (LLVM EH plus a C personality function, or
  error-return lowering). Then: the `Error` class, `throw`, `try`/`catch`/`finally` as an expression,
  typed catch clauses, and the checked-`throws` analysis with error-union narrowing.
- **Done when**: run tests pass, and fail tests cover an unhandled checked error.

### P3.20 Exceptions II
- **Spec**: 07 §7.2 (inference, polymorphism), `try?`/`try!`, lightweight `Throwable`
- **Do**: throws inference for private functions and lambdas, throws polymorphism in HOFs,
  `try?`/`try!`, and struct/enum `Throwable` (no stack trace).
- **Done when**: `lines.map(l => parseInt(l))` throws exactly `ParseError` at the call site (both
  run and fail tests).
- **Split**: lightweight `Throwable` structs/enums moved to P3.20b.

### P3.20b Lightweight throwables
- **Spec**: 07 §7.1 (lightweight throwables)
- **Do**: structs/enums that `implement Throwable` (`message(): string`) can be thrown, caught by
  type and declared in `throws`, with no allocation or stack trace. The pending-exception slot
  then holds either an `Error` object or a throwable value (needs a tagged slot or a value area).
- **Done when**: `enum ParseFail implements Throwable { … }` round-trips through `throw`/`catch`
  in run tests, and fail tests cover an unhandled one.

### P3.21 Panics and stack traces
- **Spec**: 07 §7.5
- **Do**: panic unwinding that runs `finally`, the top-level handler, stack capture
  (`backtrace`-style in C), and symbolized output in debug builds.
- **Done when**: the panic output includes a symbolized trace on macOS (snapshot with addresses
  normalized).

### P3.22 Core collections and Array API
- **Spec**: 13 §13.1 (Map/Set/Array)
- **Do**: `Map`/`Set` (hash tables built on `hashCode`/`equals`), Array methods (map, filter,
  reduce, find, some, every, sort, slice, join, includes, indexOf), and string methods (split, trim,
  startsWith, includes, replace, toUpperCase, padStart, …), written in pico where possible
  (`stdlib/core/*.pi`).
- **Done when**: run tests cover the common APIs and throws propagation through `map`.

### P3.23 Iterators and generators
- **Spec**: 05 §5.7, 04 §4.3
- **Do**: `Iterable`/`Iterator` protocols, `function*`/`yield` lowered to a state machine in MIR,
  lazy adapters (`map`, `filter`, `take`, `zip`, `enumerate`), and `for-of` over any iterable.
- **Done when**: the `fib().take(10)` example works.

### P3.24 Modules
- **Spec**: 08 §8.1–8.2, 04 §4.2
- **Do**: multi-file compilation, extensionless resolution, `import`/`export` forms (named,
  `* as`, `as`, re-export), const-only library modules (`let` → error), initialization order,
  init-cycle detection, and `std/` path mapping to the bundled `stdlib/` directory.
- **Done when**: multi-file fixtures (`tests/run/modules/<case>/main.pi`) pass, and fail tests
  cover a cycle and a library `let`.

### P3.25 Destructuring and spread
- **Spec**: 04 §4.1
- **Do**: object/array/tuple destructuring with defaults and rest, destructured parameters, and
  spread in array literals.
- **Done when**: the §4.1 and §5.3 examples are run tests.

### P3.26 `using` and Disposable
- **Spec**: 11 §11.5
- **Do**: `using` with `Disposable` (sync), reverse-order disposal on every exit path, exceptions
  included. (`await using` comes in P5.3.)
- **Done when**: run tests show the disposal order with and without exceptions.

---

## Phase 4: Memory management

### P4.1 Shadow stack and type maps
- **Spec**: 09 §9.1–9.2
- **Do**: per-type GC layout descriptors (offsets of references, including inside structs), and
  shadow-stack frames for GC roots emitted by codegen.
- **Done when**: a debug `gc.dumpRoots()` lists the expected roots in a test.

### P4.2 Mark-sweep GC
- **Do**: a precise, non-moving mark-sweep collector with a per-thread heap (still one thread),
  allocation-triggered collection, and `std/gc` `collect()`/`stats()`.
- **Done when**: an allocation stress test runs in bounded memory (it checks `gc.stats()`).

### P4.3 Generational and incremental GC
- **Do**: a nursery (bump allocation) with promotion, write barriers emitted by codegen, incremental
  old-generation marking, `pause()`/`resume()`/`collectYoung()`, and pause-time stats.
- **Done when**: the stress test passes, and a benchmark records max pause < 1 ms for a game-like
  workload (documented in PROGRESS.md).

### P4.4 Weak references and finalization
- **Spec**: 09 §9.3
- **Do**: `WeakRef`, `WeakMap`, `WeakSet`, `FinalizationRegistry` (callbacks queued for the event
  loop; until P5.1 they run at exit).
- **Done when**: run tests show a weak reference cleared after `gc.collect()`.

### P4.5 Unsafe and pointers
- **Spec**: 09 §9.5
- **Do**: `unsafe` blocks and functions, `ptr<T>` operations, `sizeof`/`alignof`, `@pod`
  (C layout, no references), and `unsafe.cast`.
- **Done when**: fail tests cover pointer use outside `unsafe` and a reference field in a `@pod` struct.

### P4.6 Arenas
- **Spec**: 09 §9.4
- **Do**: the scoped `arena { }` block (allocations redirected, escape analysis rejects references
  leaving the block) and the `Arena` object in unsafe code.
- **Done when**: the §9.4 examples are run tests, and a fail test covers an escaping reference.

---

## Phase 5: Async and concurrency

### P5.1 Event loop
- **Spec**: 10 §10.1
- **Do**: a runtime loop (kqueue on macOS first, behind an interface), timers (`setTimeout`,
  `setInterval`, `clearTimeout`), `queueMicrotask`, and exit when nothing is pending.
- **Done when**: timer-ordering run tests pass.

### P5.2 async/await
- **Spec**: 10 §10.1, 07 §7.4
- **Do**: lower async functions to MIR state machines, eager `Promise<T>` with throws tracking,
  `await` rethrowing (checked), top-level `await`, and `Promise.all`/`race`/`allSettled`.
- **Done when**: run tests cover ordering with JS-identical output, and a fail test covers an
  unhandled awaited error.

### P5.3 Async generators and `await using`
- **Do**: `async function*`, `for await`, `AsyncIterator`, and `await using` with `AsyncDisposable`.
- **Done when**: the §5.7 async example and an `await using` ordering test pass.

### P5.4 Cancellation
- **Spec**: 10 §10.5
- **Do**: `AbortController`, `AbortSignal`, `AbortError`, and `signal` support in timers.
- **Done when**: an aborted timer rejects with `AbortError` in a run test.

### P5.5 Workers
- **Spec**: 10 §10.2–10.3
- **Do**: OS threads, each with its own heap and module instances, `Worker<In, Out>`,
  `postMessage`/`onmessage`, the Sendable check (compile error), deep copy (cycles preserved),
  `terminate`, and panic → `onerror`.
- **Done when**: a ping-pong test passes, and a fail test covers sending a closure.

### P5.6 Transferables, inline workers, pools
- **Spec**: 10 §10.3–10.4
- **Do**: `ArrayBuffer`/typed arrays (off-heap) with `transfer` and detaching, inline capture-free
  workers with `w.call()`, `parallelMap`, and `WorkerPool`.
- **Done when**: a `parallelMap` hashing test passes, and a fail test covers an inline worker that
  captures.

---

## Phase 6: Comptime and metaprogramming

### P6.1 Constant folding
- **Do**: fold pure constant expressions in MIR, with overflow during folding reported as a compile error.
- **Done when**: the MIR dump shows `const KB = 1 << 10` folded.

### P6.2a `comptime` evaluation
- **Spec**: 11 §11.1
- **Do**: `comptime const`, `comptime { }`, `comptime if` run via `pico-interp`;
  `target`/`build` info, `comptime.readFile`, and the 10M step limit with `@comptimeLimit`.
- **Done when**: the CRC table example works, and a fail test covers an infinite loop hitting the
  step limit.

### P6.2b `comptime` parameters
- **Spec**: 11 §11.1
- **Do**: `function f(comptime n: i32)` and `function vec<comptime N: i32>()`: each distinct
  constant argument instantiates the function (like monomorphization), with the parameter a
  constant inside; non-constant arguments are an error. Also: local `comptime const`s usable
  from later comptime code in the same function.
- **Done when**: `zeros<4>()` from TOUR works and a non-constant argument fails.

### P6.3 Types as comptime values
- **Do**: `comptime T: type` and type reflection at comptime (fields, methods).
- **Done when**: a generic `vec<comptime N>` example works.

### P6.4 Decorators and std/meta
- **Spec**: 11 §11.2
- **Do**: `decorator function`, the `ClassDecl`/`FunctionDecl`/… API, adding members, wrapping
  functions, and diagnostics through `decl.error/warn/note`.
- **Done when**: the `@memoize` and `@mustHaveId` examples work.

### P6.5 Quasi-quote templates and the AST builder
- **Do**: backtick code templates parsed and type-checked during expansion, plus the `ast.*` builder.
- **Done when**: a decorator that generates a `toString()` method passes.

### P6.6 `@derive` in pico
- **Do**: reimplement `@derive(Eq, Hash, Ord)` as a stdlib decorator and remove the built-in.
- **Done when**: the P3.17 tests still pass.

### P6.7 Runtime reflection
- **Spec**: 11 §11.3
- **Do**: whole-program, on-demand metadata emission, and `reflect.typeOf` (fields, methods,
  interfaces, decorators, `get`/`set`).
- **Done when**: the §11.3 example works, and binary size is unchanged when reflection isn't used.

### P6.8 JSON
- **Spec**: 11 §11.4
- **Do**: compile-time generated `JSON.parse<T>` (validation with precise error paths) and
  `JSON.stringify`, plus `@json.name`.
- **Done when**: the Config example passes, including error messages.

---

## Phase 7: Standard library (async I/O)

### P7.1 std/fs and std/path
Split into P7.1a (everything but `watch`) and P7.1b (`watch`).

#### P7.1a std/fs and std/path
- **Spec**: 13 §13.2–13.4
- **Do**: async file operations (thread-pool backed in the runtime), `File` as `AsyncDisposable`,
  `readDir` as an async iterator, and `std/path`.
- **Done when**: run tests use a temp directory.

#### P7.1b fs.watch
- **Spec**: 13 §13.2
- **Do**: `watch(path)` as an async iterator of change events (FSEvents/kqueue on macOS,
  inotify on Linux, polling in the interpreter if needed), registered with the loop's poller.
- **Done when**: a run test sees create/modify/remove events in its temp directory.

### P7.2 std/process
- **Spec**: 13 (External processes)
- **Do**: args, env, exit, cwd, stdio streams, and `Command` (spawn, output, pipes, status, kill, env,
  cwd, no shell) plus `Command.shell`, implemented with POSIX spawn (macOS/Linux) and
  `CreateProcessW` (Windows, verified in P8.3).
- **Done when**: run tests spawn `echo` and `cat` with pipes.

### P7.3 std/net
- **Do**: TCP listen/connect, UDP, and DNS on the event loop.
- **Done when**: a TCP echo test runs server and client in one process.

### P7.4 Web Streams and encoding
- **Do**: `ReadableStream`/`WritableStream`/`TransformStream`, `TextEncoder`/`TextDecoder`, and
  `std/encoding` (base64, hex).
- **Done when**: piping through a TransformStream works in a run test.

### P7.5 fetch and std/http
Split into P7.5a (plain HTTP), P7.5b (TLS) and P7.5c (WebSocket).

#### P7.5a URL, fetch and serve over HTTP
- **Do**: `URL`, `URLSearchParams`, `Headers`, `Request`, `Response` (`json<T>()`), an HTTP/1.1
  client `fetch` (`http:`), and `std/http` `serve`.
- **Done when**: a `serve` + `fetch` round-trip test passes.

#### P7.5b TLS (https)
- **Do**: TLS for `fetch` (`https:`) and optionally `serve`, via a bundled library or the
  platform's (**the author decides which**: see PROGRESS.md), with certificate verification
  against the system roots.
- **Done when**: `fetch("https://…")` works against a local TLS server in a run test.

#### P7.5c WebSocket
- **Do**: the `WebSocket` client global and `std/http` upgrades (`upgradeWebSocket(req)`),
  frames, masking, ping/pong, close.
- **Done when**: a `serve` + `WebSocket` echo test passes.

### P7.6 Regex
- **Spec**: 01 (regex literals)
- **Do**: the `RegExp` engine (port or bind one, decided at task time), compile-time validation of
  literals, and string methods that take regexes.
- **Done when**: an invalid regex literal gives a compile error, and common JS regex tests pass.

### P7.7 Time
- **Do**: `std/time` (`Instant`, `Duration`, `DateTime`, time zones), the `Date` global, and
  `performance.now()`.
- **Done when**: run tests cover formatting and arithmetic.

### P7.8 Crypto
- **Do**: `crypto.getRandomValues`, `randomUUID`, hashes, HMAC, AEAD, and signatures (bind a vetted C
  library).
- **Done when**: known-answer tests pass.

### P7.9 Math, collections, fmt
- **Do**: `std/math` (Vec2/3/4, Mat4, Quat, bigint, random), `std/collections`, and `std/fmt`
  (`Formattable` hook for user types in `${x:…}`).
- **Done when**: run tests pass.
Split into P7.9a (`Math` and `std/math`), P7.9b (`std/collections`) and P7.9c (`std/fmt`).

#### P7.9a Math and std/math
- **Do**: the `Math` global, `std/math` (Vec2/3/4, Mat4, Quat, BigInt, Random, helpers).
- **Done when**: run tests pass, `Math` agrees with Node.

#### P7.9b std/collections
- **Do**: `Deque`, `PriorityQueue`, `SortedMap`, `BitSet`.
- **Done when**: run tests pass.

#### P7.9c std/fmt
- **Do**: `Formattable` (user types in `${x}` and `${x:spec}`, structs too) and the padding and
  number formatting helpers.
- **Done when**: run tests pass.

### P7.10 std/test and `pico test`
- **Spec**: 14 §14.3
- **Do**: `describe`/`test`/`expect` matchers (`toBe`, `toEqual`, `toThrow`, `toPanic`, `rejects`),
  hooks, `*.test.pi` discovery, one worker per file, `--watch`, and `bench` with `pico bench`.
- **Done when**: the stdlib's own tests are rewritten as `*.test.pi` and run in CI.

---

## Phase 8: Targets

### P8.0 Linux host bring-up
- **Spec**: 15 §15.3
- **Do**: make `scripts/test.sh` green on Linux arm64 (`aarch64-unknown-linux-gnu`), the new
  development host. The Linux branches of the runtime were written but never compiled: epoll in
  `pico_loop.c`, inotify in `pico_io.c`, the TLS CA bundle paths in `pico_tls.c`, and the `-lm` /
  `-rdynamic` link flags in `pico-driver`. Enable `panic_trace.rs` on Linux (glibc `dladdr`/
  backtrace symbolization). Drop `continue-on-error` from the Linux CI job.
- **Done when**: `scripts/test.sh` passes on the Linux VM and both CI jobs (macOS, Linux) are green.

### P8.1 Cross-compilation infrastructure
- **Spec**: 15
- **Do**: target triples and aliases, a `--target` flag, per-target runtime builds (clang), linking
  through **lld** (via LLVM, no system linker), and the glibc version suffix.
- **Done when**: building `linux-x64` from the Linux arm64 host produces an x86-64 ELF binary
  (verified with `file`) that runs under `qemu-user`, linked by lld without the system `cc`.

### P8.2a glibc stubs
- **Spec**: 15 §15.2
- **Do**: vendor glibc's ABI list and headers (from Zig) and generate versioned stub libraries per
  requested glibc version (`.2.xx` suffix, default 2.31), so `linux-gnu` builds need no system
  glibc. Our own start files replace glibc's crt objects; whatever `libc_nonshared.a` provided
  that the runtime needs is ours too. (epoll is in the event loop since P8.0.)
- **Done when**: the golden tests pass natively on the Linux arm64 host and for `linux-x64` under
  `qemu-user`, linked against the generated stubs only, and a binary built for 2.31 references
  no newer symbol version.

### P8.2b musl
- **Spec**: 15 §15.1
- **Do**: vendor musl's sources (`third_party/musl`), compile `libc.a` per target with clang on
  first use (cached like the runtime) and link `*-linux-musl` targets fully statically.
- **Done when**: the golden tests pass natively for `linux-arm64-musl` and for `linux-x64-musl`
  under `qemu-user`.

### P8.3a Windows toolchain and runtime core
- **Spec**: 15 §15.1, §15.2
- **Do**: lld's COFF/MinGW driver in `pico-lld`; MinGW from Zig's mingw-w64 (CRT start code,
  import libraries generated from `.def` files, winpthreads), with the Windows headers pruned to
  what the runtime and Mbed TLS include (a script regenerates the set); codegen for
  `x86_64`/`aarch64-w64-windows-gnu` with CodeView debug info; the runtime core on Windows (GC
  memory, threads and workers, the event loop on IOCP with timers, panics). I/O modules report
  "not supported on Windows yet".
- **Done when**: the golden tests that need no I/O pass for `windows-x64` and `windows-arm64`
  on GitHub's `windows-latest` and `windows-11-arm` runners (binaries built by the Linux job,
  run there).

### P8.3b Windows files
- **Do**: std/fs, `watch` (`ReadDirectoryChangesW`) and encoding ops on Windows.
- **Done when**: the fs golden tests pass for `windows-x64` and `windows-arm64` in CI.

### P8.3c1 Windows processes, environment and crypto
- **Do**: std/process on Windows: `CreateProcessW` with pipe handles (the pipe jobs on their own
  threads, as on POSIX), waiting, `kill` (`TerminateProcess`), `Command.shell` through
  `cmd.exe /c`, the environment, arguments, `exit`, `pid`; Mbed TLS and `std/crypto` on Windows;
  Windows process fixtures (the POSIX ones use `sh`, `cat` and friends).
- **Done when**: `run/process/` (or its Windows counterparts), `run/crypto/` and
  `run/test/runner.pi` pass for `windows-x64` and `windows-arm64` in CI.

### P8.3c2 Windows sockets and TLS
- **Do**: std/net on Winsock (blocking calls on the job threads, as on POSIX; the loop already
  waits on a completion port), `pico_tls.c` on Winsock, and the Windows certificate store for
  `fetch`.
- **Done when**: all golden tests pass for `windows-x64` and `windows-arm64` in CI, except
  those needing named time zones (no tz database on Windows yet).

### P8.3d Bundled IANA tz database
- **Spec**: 13 (std/time)
- **Do**: vendor the IANA tzdb (data and `zic`; a script regenerates compact TZif files), link
  them into programs for targets without a system database (Windows; WASI later), look zones
  up there when the system has none, and find Windows' local zone (Windows zone names mapped to
  IANA ids with CLDR's `windowsZones` table). The author chose bundling.
- **Done when**: `run/time/std_time`, `run/time/date` and `run/fmt/formattable` pass for
  `windows-x64` and `windows-arm64` in CI (nothing left in `tests/windows-skip.txt`).

### P8.4 macOS cross builds
- **Do**: bundled `.tbd` stubs, x64 and arm64 from any host, and universal-binary support (optional).
- **Done when**: both architectures build from the Linux host (Mach-O verified with `file`), and on
  macOS arm64 (the VM's host or the CI runner) the arm64 binary runs and the x64 one runs under
  Rosetta.

### P8.5a wasm32: core programs under wasmtime
- **Spec**: 12 §12.5
- **Do**: target-dependent layouts (4-byte pointers: `Program::size_align`, the GC's type
  maps), wasi-libc from Zig compiled by pico (WASI preview 1 core modules for now), the runtime on
  wasm32 (no threads: `Worker` and `Command` are compile errors there; `try` and panics through
  Wasm exception handling), linking with lld's wasm driver.
- **Done when**: the golden tests that need no I/O pass under wasmtime (installed user-local,
  same version in CI).

### P8.5b wasm32: files, clocks and the event loop
- **Do**: std/fs, clocks, random and timers on WASI (the loop waits in `poll_oneoff`), with the
  preopened working directory.
- **Done when**: the fs, time and async golden tests pass under wasmtime.

### P8.5c wasm32-wasip2: components, sockets and http
- **Do**: the `wasm32-wasip2` target as a component (preview 2), sockets and `fetch`/`serve` on
  `wasi:sockets` and `wasi:http`.
- **Done when**: all golden tests available on WASI pass under wasmtime.

### P8.6 wasm32-browser (linear memory)
- **Spec**: 12 §12.3
- **Do**: wasm32-unknown-unknown with the pico GC, ESM glue `app.js`, `@export("js")`, a generated
  `.d.ts`, structs crossing as plain objects, classes as handles, and the event loop bridged to the
  browser.
- **Done when**: golden tests run in node/deno through the glue, and a demo page works.
- **Done (decision 258)**: linear memory, `@export("js")` for numbers, `bool`, `string`, structs
  and class handles; arrays, enums and nullable types at the boundary are left for later.

### P8.7 WasmGC backend
- **Do**: a separate wasm emitter from MIR that uses WasmGC types (LLVM can't do this), so pico
  objects become browser-GC objects.
- **Done when**: the golden tests pass on the WasmGC build, with a measured size/perf comparison.
- Split into slices (`PICO_BACKEND=wasmgc` with `--target wasm32-browser`; what a slice can't
  compile is pending in the golden suite):

#### P8.7a Emitter skeleton
- **Do**: `wasm-encoder` module from MIR: numbers with pico's checks, `bool`, `char`, strings
  (`(array i8)`), structs and classes as GC structs (no virtual calls), globals, calls, panics,
  printing and number formatting through the glue.
- **Done when**: the fixtures that use only that pass, the rest is pending, none fails.

#### P8.7b Strings and arrays
- **Do**: string indexing/slicing and the core library's string code, growable and fixed arrays,
  ranges, tuples, format specs, `T?` of values.
- **Done (decision 266)**: 39 run fixtures pass on the WasmGC backend, none fail.

#### P8.7c Objects
- **Do**: virtual dispatch (vtables in class structs), interfaces (itables), closures (funcrefs
  and environments), enums and unions, `instanceof`, the string form of objects.
- **Done (decision 267)**: with exceptions' error-return slot (from P8.7d); 59 run fixtures pass.

#### P8.7d Errors and the event loop
- **Do**: exceptions and panic guards through Wasm exception handling, generators, async and the
  event loop on browser timers, `@export("js")`, `std/js`.
- **Done (decision 268)**: without `@export("js")`/`std/js` on WasmGC (moved to P8.7e);
  `tests/wasmgc-skip.txt` for what needs linear memory.

#### P8.7e Coverage and comparison
- **Do**: the remaining core library builtins (`__io` ops that make sense in a browser, math,
  crypto through WebCrypto or a port), then the size/perf comparison with the linear-memory
  build.
- **Done (decision 269)**: 193 of the suite pass on WasmGC, none fail, the rest is skip-listed
  (crypto stays on Mbed TLS, linear memory only); `scripts/wasm-compare.sh`.

#### P8.7f JavaScript interop on WasmGC
- **Do**: `@export("js")` (references passed to JavaScript as they are, numbers and strings
  converted by the glue) and `std/js` (`JsValue` handles, `js:` imports, callbacks into pico)
  on the WasmGC build.
- **Done when**: `examples/browser` and `examples/browser-dom` work with `PICO_BACKEND=wasmgc`.
- **Done (decision 270)**: `tests/browser` runs on both backends; `JsValue` handles aren't
  released on WasmGC yet (no weak references there).

### P8.8 JS imports from .d.ts
- **Spec**: 12 §12.3
- **Do**: a `.d.ts` subset parser, best-effort mapping (handles, simple shapes → structs, else
  `JsValue`), `js:` specifiers, and `std/js` `JsValue`.
- **Done when**: a DOM demo using `lib.dom.d.ts` builds and runs.
- **Done (decision 259)**: `examples/browser-dom` (checked in headless Chromium); promises and
  object shapes as structs are left for later.

### P8.9 Platform files and conditionals
- **Spec**: 15 §15.4, 13 (matrix)
- **Do**: `x.linux.pi`/`x.wasm.pi` resolution, `target.*` at comptime, and the stdlib availability
  matrix enforced with clear errors.
- **Done when**: fail tests cover importing `std/fs` for the browser.
- **Done (decision 260)**: `tests/fail/targets/`, `tests/run/modules/platform_files`.

---

## Phase 9: C interop

### P9.1 extern "C"
- **Spec**: 12 §12.1
- **Do**: `extern "C"` blocks, the type mapping table, `@link`, automatic string → C-string
  conversion, the safe/unsafe call rules, and callbacks.
- **Done when**: calling libc `strlen`/`qsort` works, and a fail test covers a pointer call outside
  `unsafe`.
- **Done (decision 261)**: `tests/run/ffi/libc.pi`, `tests/fail/ffi/`; `@pod` structs by value
  are left for later (by pointer works).

### P9.2 C header import
- **Spec**: 12 §12.2
- **Do**: `c:` specifiers parsed through libclang (clang-sys), generated bindings (functions, structs,
  enums, literal macros), and include/link settings from `project.json`/pkg-config.
- **Done when**: `import * as m from "c:math.h"` works, and an SDL2 or raylib demo opens a window
  (manual check).
- **Done (decision 262)**: through clang's JSON AST rather than libclang; `c:math.h` and a local
  header in the golden suite, zlib in `crates/pico-tests/tests/c_headers.rs`; the SDL2 demo
  (`examples/sdl2`) checked by hand (window confirmed by the author, 2026-11-03).

### P9.3 Export to C
- **Spec**: 12 §12.4
- **Do**: `@export("c")`, `--lib shared|static`, `.h` generation, and lazy heap initialization for
  threads calling in from C.
- **Done when**: a C program links against a pico static library and calls it.
- **Done (decision 263)**: `tests/c/export` (static and shared, from two threads) in
  `crates/pico-tests/tests/c_headers.rs`; `.dylib`/`.dll` are left for later.

---

## Phase 10: Tooling and ecosystem

**Order** (the author's, 2026-11-03): first what leads to the VS Code integration, then debugging
(which adds debugging in VS Code), then the rest, release packaging last:
P10.6a → P10.6b → P10.6c1 → P10.6c2 → P10.6d1 → P10.6d2 → P10.2 → P10.5 → P10.8 → P10.3 → P10.4 → P10.9 →
P10.10. (Task ids stay as they were: decisions and the log refer to them.)

### P10.1 Formatter
- **Spec**: 14 §14.4
- **Do**: a lossless CST or trivia-preserving printer, the configured options, and `pico fmt --check`.
- **Done when**: formatting every fixture is idempotent and the fixtures are formatted in CI.
- **Done (decision 265)**: token-based (lossless); breaking long lines (`lineWidth`) is left for
  later.

### P10.2 Lint
- **Spec**: 14 §14.7
- Split into two slices:

#### P10.2a `pico lint`: the command, configuration, naming and unused code
- **Do**: `pico lint` over the project's modules (W0xxx warnings, exit status 1 when there are
  any), `project.json`'s `"lint"` (each rule `false`/`true`/options), the `naming` and `unused`
  rules, and the language server publishing lint warnings with the diagnostics.
- **Done when**: snapshot tests of the rules and the configuration pass.
- **Done (decision 278)**: `crates/pico-lint` (`config`, `walk`, `naming`, `unused`), `pico lint`,
  warnings in `pico-lsp`'s diagnostics; snapshots in `crates/pico-lint/src/snapshots`.

#### P10.2b Optional rules: complexity, blank lines, docs
- **Do**: the `complexity` (default 4), `blankLines` (`before` list) and `docs` (exported
  declarations and public members of exported types; inherited docs count) rules, off unless
  enabled.
- **Done when**: snapshot tests of each rule, enabled, disabled and with options, pass.
- **Done (decision 278)**: `complexity.rs`, `blank.rs`, `docs.rs` in `crates/pico-lint`.

### P10.3 Package manager
- **Spec**: 08 §8.3–8.4
- **Do**: the `project.json` schema, `pico init`/`add`/`remove`, git dependencies, semver resolution
  (majors coexist), `project.lock` with hashes, and workspaces.
- **Done when**: a two-package workspace with one git dependency builds offline from the lockfile.
- Split into slices (decisions 284–286):

#### P10.3a Manifest, package imports, workspaces, `entry`
- **Do**: the manifest (validated), `import … from "scope/name"` → the package's `index.pi`
  (sub-paths an error), workspace members as dependencies by name and semver range, semver
  parsing and matching, `pico build`/`pico run` without a file building `entry`, and VS Code's
  debugger building the nearest project's `entry`.
- **Done when**: a two-member workspace (an application using a library by name) builds and
  runs; tests for ranges, errors (no `index.pi`, a sub-path, an unsatisfied range).
- **Done**: `crates/pico-driver/src/semver.rs`, `packages.rs` (manifest, `resolve`, `entry_of`),
  the loader's package specifiers and `dev_root`; `pico build`/`pico run` without a file; the
  extension's F5 and build task, `pico dap` with a directory; tests
  `crates/pico-driver/tests/packages.rs`.

#### P10.3b Git dependencies, the lockfile, offline builds
- **Do**: git sources (tag/branch/rev) cloned by commit into the cache, `project.lock` (sources,
  commits, `sha256:` hashes), unification by major (majors coexist), offline builds from the
  lockfile and cache (hash checks), `pico fetch`, `devDependencies` for tests.
- **Done when**: a two-package workspace with one git dependency builds offline from the lockfile.
- **Done**: `crates/pico-driver/src/git.rs` (mirror, `resolve`, `checkout`, `content_hash`),
  `packages::{Lock, prepare, Prepared}`, the loader resolving git dependencies through it,
  `pico fetch`; tests in `crates/pico-driver/tests/packages.rs` (offline build, tampering, an
  out-of-date lockfile, majors unifying or coexisting).

#### P10.3c Commands
- **Do**: `pico init [name] [--lib]`, `pico add <git-url> [--tag/--branch/--rev] [--dev]`,
  `pico remove <name>`.
- **Done when**: tests drive the commands on a scratch project (manifest and lockfile after each).
- **Done**: `packages::{init, add_git, remove}`, `git::tags`; `pico init/add/remove` in the CLI;
  test `crates/pico-cli/tests/packages.rs`.

### P10.4 Registry
- **Do**: the index format (`scope/name.json`), sparse HTTPS fetching, `pico publish` (tag + PR via
  `gh`), and registry override.
- **Done when**: publishing to a test index repo, then installing from it, works.
- **Deferred** (the author, 2026-11-03): git dependencies (also over SSH, with the system git's
  credentials) cover sharing libraries for now; the registry comes later.

### P10.5 Incremental compilation
- **Spec**: 14 §14.6
- Split into two slices (decision 279: measured, parse + resolve + typecheck take ~20 ms of a
  check, and the whole-program checker can't cache per module cheaply):

#### P10.5a Build fingerprints
- **Do**: `pico build` records each output's inputs (compiler, options, target, the content of
  the entry, imported and std modules, comptime-read files, `project.json`) and skips the build
  when nothing changed; `--force`; C/JS imports and browser builds always build.
- **Done when**: a no-op rebuild of a 50-module project takes < 200 ms (measured), and tests show
  that changing a module, an option or the output rebuilds.
- **Done**: `crates/pico-driver/src/fingerprint.rs`, `pico build --force`, `comptime::take_files_read`;
  test `crates/pico-driver/tests/fingerprint.rs`. Measured on a generated 50-module project:
  no-op rebuild 2 ms (full build 190 ms, release 250 ms).

#### P10.5b Cached code generation
- **Do**: emit the core library and each module as object files of their own, cached by content
  (and the instances they need), so a change rebuilds only its module's code.
- **Done when**: measured on a large program (to be chosen): a one-module change rebuilds faster
  than a full build.

### P10.6 LSP and VS Code extension
- **Do**: `pico lsp` (diagnostics, hover, go-to-definition, completion, rename, format), plus a
  minimal VS Code extension with a TextMate grammar.
- **Done when**: a manual check in VS Code, plus LSP protocol tests.
- Split into slices:

#### P10.6a VS Code extension: syntax
- **Do**: `editors/vscode`: a declarative extension (no code yet) with the `pico` language
  (`.pi`), a TextMate grammar (keywords, built-in types, literals, template strings with
  `${…}`, comments and doc comments, decorators, declarations) and the language configuration
  (comments, brackets, auto-closing, indentation).
- **Done when**: a test keeps the grammar in sync with the lexer's keywords and its patterns
  valid, and a manual check in VS Code.
- **Done**: the grammar is generated (`syntaxes/generate.py`); checked with `vscode-textmate`
  on sample files during development; the manual check in VS Code is the author's.

#### P10.6b `pico lsp`: diagnostics and formatting
- **Do**: a stdio JSON-RPC server (initialize, text sync of open documents, shutdown),
  diagnostics from `check_source` on open/change (debounced), `textDocument/formatting`
  (P10.1), and the extension's language client starting `pico lsp`.
- **Done when**: LSP protocol tests (a scripted session over stdio) pass.
- **Done (decision 271)**: `crates/pico-lsp` (tests in `src/tests.rs`), the client in
  `editors/vscode/extension.js`; also checked against `vscode-jsonrpc` during development.

#### P10.6c Hover and go-to-definition
- Split into two slices:

#### P10.6c1 Symbol index, hover and go-to-definition
- **Do**: an index from source positions to resolved symbols (declarations, uses, types) built
  from the HIR and `Types`; hover (type and `/** */` docs), go-to-definition across modules
  and into the stdlib/prelude, on code that checks cleanly (the server keeps the last good
  index of a document while it has errors).
- **Done when**: protocol tests on error-free fixtures (locals, parameters, functions, types,
  members, imported names, a prelude declaration).
- **Done (decision 272)**: `crates/pico-lsp/src/index.rs`, `Program::name_refs` in the resolver;
  protocol tests in `crates/pico-lsp/src/tests.rs`.

#### P10.6c2 Error recovery for the language server
- **Do**: the driver keeps resolving and checking past errors (for the language server at
  least), the resolver and checker accept `Error` nodes and unresolved names without cascades
  of follow-up errors, so hover and go-to-definition keep answering in half-typed code.
- **Done when**: protocol tests on fixtures with syntax and type errors, and `tests/fail`
  still reports the same first errors.
- **Done (decision 273)**: `pico_driver::check_source_recovering`, used by the server only when
  the normal check stops before types; the compiler's pipeline is unchanged.

#### P10.6d Completion and rename
- Split into two slices:

#### P10.6d1 Completion
- **Do**: `textDocument/completion` (trigger `.`): names in scope at the cursor (locals,
  parameters, the file's top-level declarations, imported names, the core library, built-in
  types and functions, keywords), members after `.` (fields, methods, getters; the methods of
  built-in types), and the exported names of the module inside `import { … } from "…"`; each
  with its kind and signature. Completion runs the recovering front end on the text with a
  placeholder name at the cursor.
- **Done when**: protocol tests, and a manual check in VS Code.
- **Done (decision 274)**: `crates/pico-lsp/src/complete.rs`.

#### P10.6d2 Rename and document symbols
- **Do**: rename (all uses of a local, a member or a top-level declaration, across the
  project's modules, also those that import the renamed one; `prepareRename` refuses the core
  library and built-ins; the new name must be an identifier), document symbols (the outline).
- **Done when**: protocol tests, and a manual check in VS Code.
- **Done (decision 275)**: `crates/pico-lsp/src/rename.rs`, `symbols.rs`; links, shorthand
  fields and method families in the index.

### P10.8 Debugging
- **Spec**: 14 §14.5
- **Do**: DWARF/CodeView emission, `pico dap` built on lldb-dap, pretty-printers for pico values,
  and `pico symbolize` for release traces.
- **Done when**: breakpoints and value display work in VS Code (manual check).
- Split into slices (lldb-23 and lldb-dap-23 are installed on the Linux host):

#### P10.8a Line tables
- **Do**: DWARF compile unit, files and a subprogram per function, and a source location for every
  statement and terminator, in every file (modules and the core library too: MIR locations
  carry the file); `pico build` emits them unless `--release`.
- **Done when**: an lldb batch test (skipped without lldb) stops at a breakpoint set by
  `file:line` and its backtrace shows the pico functions with their files and lines.
- **Done (decision 280)**: `Dbg`/`begin_debug`/`debug_loc` in `pico-codegen`, `mir::Program::sources`
  (filled by the driver), `Options::debug`, `dsymutil` on macOS; test
  `crates/pico-driver/tests/debug_info.rs`.

#### P10.8b Variables
- **Do**: parameters and locals as DWARF variables with their types (integers, floats, `bool`,
  `char`, strings, arrays, structs, classes, enums, nullables) and scopes.
- **Done when**: an lldb batch test shows the variables of a frame with their values.
- **Done (decision 281)**: `declare_variables`, `di_type` in `pico-codegen`; test `variables` in
  `crates/pico-driver/tests/debug_info.rs`.

#### P10.8c Debugging in VS Code
- **Do**: lldb formatters (Python) that show pico values (strings, arrays, maps, enums,
  nullables, objects' fields), `pico dap` (lldb-dap with them loaded), and the extension's
  debugger (a launch configuration that builds and debugs a `.pi` program).
- **Done when**: breakpoints, stepping and value display work in VS Code (manual check).
- **Done (decision 282)**: `crates/pico-cli/src/dap.rs`, `pico_lldb.py`, the extension's debugger;
  test `crates/pico-cli/tests/dap.rs` (a scripted DAP session).

#### P10.8d `pico symbolize`
- **Do**: release builds keep a separate debug file; `pico symbolize` turns a release stack
  trace's addresses into functions, files and lines.
- **Done when**: a test symbolizes a release program's panic trace.
- **Done (decision 283)**: release builds link twice (`app.dbg` with DWARF, `app` stripped, one
  build ID), `dsymutil` + `strip -S` on macOS; the runtime prints offsets and the build ID;
  `crates/pico-cli/src/symbolize.rs` (addr2line); test `crates/pico-cli/tests/symbolize.rs`.

### P10.9 Documentation tooling
- **Do**: `pico doc` (HTML from `/** */`) and `pico explain E####` for every error code.
- **Done when**: stdlib docs are generated.
- Split into two slices:

#### P10.9a `pico explain`
- **Do**: a long-form explanation for every error and lint code (`docs/errors/E0308.md`: what
  it means, an erroneous example, the fix), embedded in `pico`; `pico explain E0308` prints it,
  and the compiler's messages mention it.
- **Done when**: a test checks every explanation exists and its examples are real: each
  erroneous example reports its code, each fixed one compiles.
- **Done (decision 287)**: `docs/errors/*.md` (83 error and 5 lint codes), embedded by
  `pico-diag/build.rs` (`codes::explain`, `codes::parse`); `pico explain`; the CLI's hint after
  errors; test `crates/pico-tests/tests/explain.rs`.

#### P10.9b `pico doc`
- **Do**: HTML from doc comments: an index and a page per module, each exported declaration's
  signature (as written) and its rendered Markdown, members of types, links between pages;
  `pico doc --std` for the standard library and the core library, `pico doc` for the current
  project's API (its `index.pi`).
- **Done when**: stdlib docs are generated (and a test checks a page's declarations and links).

### P10.10 Release packaging
Split (2026-11-05): P10.10a releases archives that need an LLVM installation; P10.10b is the
single binary.

#### P10.10a Release archives
- **Spec**: 14 §14.6
- **Do**: the compiler finds its resources next to the executable (`share/pico`) or in
  `PICO_HOME`; `scripts/package.sh` builds `pico-<version>-<host>.tar.gz`; a release workflow
  builds macOS arm64 and Linux x64/arm64 archives on a tag, tries them on clean runners and
  publishes a GitHub release.
- **Done when**: on a runner with only LLVM 23 installed, the extracted archive builds and runs
  a host program and a `wasm32-wasi` one (decision 296).

#### P10.10b Single binary
Split (2026-11-05) into three:

##### P10.10b1 Embedded resources
- **Spec**: 14 §14.6
- **Do**: the runtime, vendored C sources, stdlib and host runtime libraries bundled into
  `pico` (feature `embed`, used by release builds) and unpacked into the cache on first use; the
  archive is `bin/pico` plus license files.
- **Done when**: the binary alone, with an empty cache, builds and runs programs (decision 297).

##### P10.10b2 No LLVM installation
- **Spec**: 14 §14.6
- **Do**: clang in process (its static libraries through a shim, like lld's) for the runtime,
  libc builds and `c:` imports, with its resource headers bundled; z3, zstd and libxml2 linked
  statically or left out of the LLVM build, so the binary needs only the system libraries.
- **Done when**: on a runner without LLVM, the release archive builds and runs a host program
  and a `wasm32-wasi` one.

##### P10.10b3 More hosts
- **Spec**: 14 §14.6
- **Do**: Windows x64/arm64 and macOS x64 hosts in the release workflow; how pico documents
  the mingw-w64 ZPL notice for programs it builds for Windows.
- **Done when**: every host archive passes the release workflow's try job.

---

## Phase 11: Reactivity
Vue-like reactive state (the author, 2026-11-03): `reactive` declarations of `Reactive<T>`,
`computed(() => …)` / `computed({ get, set })` and `watchEffect(() => …)`.

### P11.1 Spec
- **Do**: the `reactive` declaration and `Reactive<T>`; reads and writes; `computed` (getter,
  or `{ get, set }`) returning a `Reactive<T>`; `watchEffect`; dependency tracking, caching and
  invalidation; a spec chapter. The author's answers (decisions 288–293):
  - **Access** is transparent: `reactive n = 1`, read as `n + 1`, written as `n = 2`.
  - **Scope**: local and module-level bindings, and class fields (`reactive count = 0`).
  - **Depth**: deep: mutating what a reactive holds (`xs.push(1)`, `user.name = …`,
    `map.set(…)`) triggers updates, as reassigning does.
  - **Timing**: `computed` is lazy: a write marks it dirty, it's recomputed when read and
    cached until a dependency changes; `watchEffect` (which nothing reads) runs synchronously
    during the write that triggers it.
  - **Workers**: `Reactive<T>` isn't `Sendable`: reactive state is never shared with workers.
  - **Cleanup**: `watchEffect` returns a `Disposable` handle that stops it, and effects stop
    when the scope that created them ends.
- **Done when**: the spec chapter and its decisions are committed.

### P11.2 Core runtime
- **Do**: in the core library (pico): the dependency graph, the stack of the effect being
  tracked, lazy cached `computed` values with dirty marking, effect scheduling, cleanup through
  `Disposable`/`using`.
- **Done when**: golden tests of tracking, caching, invalidation and cleanup pass.

### P11.3 Compiler support
- **Do**: the `reactive` keyword, and typing `Reactive<T>` reads and writes (transparent access
  where the spec says so).
- **Done when**: golden and fail tests of the syntax and typing pass.

### P11.4 Effects in the event loop
- **Do**: batching, and when `watchEffect` runs again (synchronously or in a microtask, per the
  spec).
- **Done when**: golden tests of batching and ordering pass (interpreted and native).

### P11.5 Tooling
- **Do**: reactive values in the debugger (lldb formatters), hover showing `Reactive<T>`,
  examples in TOUR.
- **Done when**: tests of the formatter and hover, and a manual check in VS Code.

---

## Phase 12: Later
- **P10.7a** (moved from Phase 10 by the author): `pico run` through LLVM's ORC JIT (no object
  file, no linking; the C runtime available to the JIT). Done when `pico run` on a script is
  faster than an AOT build and run.
- **P10.7b** (moved from Phase 10 by the author): a REPL (`pico` with no arguments) that keeps
  declarations and variables between inputs and continues incomplete statements on the next
  line. Done when it handles multi-line input and redefinitions.
- **P12.1** `--no-gc` subset (`str`, `StrBuf<N>`, lightweight throwables only) and a bare-metal
  target.
- **P12.2** riscv64.
- **P12.3** Android (`aarch64-linux-android`).
- **P12.4** iOS (`aarch64-apple-ios`).
- **P12.5** Self-hosting investigation (rewrite parts of the compiler in pico).
