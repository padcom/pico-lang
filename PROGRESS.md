# Progress

> **Starting a new session?** CLAUDE.md already tells the agent what to do, so *"continue"* is
> enough. Or say it explicitly: *"Do the next task from PROGRESS.md."*

## Next task
**P5.6: Transferables, inline workers, pools** (see [PLAN.md](PLAN.md#p56-transferables-inline-workers-pools)).

Notes:
- Pipeline: `ppl_driver::check_source` runs parse → resolve (HIR) → typecheck (`Types`) → MIR;
  `run_interp` and `build_native` run it. New features touch: resolver (`ppl-sema/src/resolve.rs`,
  replace an E0900 "not implemented"), typeck, MIR lowering, the interpreter **and** codegen (+ C
  runtime when needed). Golden fixtures run interpreted and native; they must agree.
- Value representation so far: primitives in registers/allocas, strings as `PplStr*` (see
  `runtime/include/ppl_rt.h`). Structs (P3.4) are the first aggregate: decide their MIR/LLVM
  layout (inline values, field places) and make `Place` support fields.
- Integer literals without context default to `int`; there are no inference variables, so
  `const g = if (c) 1 else 2` is `int` even if later returned as `i32` (by design, §1.3).
- Commit only through a `scripts/test.sh` exit-status check. When editing files with scripts,
  assert that anchors exist and slices are non-empty (an empty-slice replace once corrupted
  `ppl_rt.c`; it was restored from git).

## Current state
| Area | State |
|------|-------|
| Spec | ✅ 16 chapters, 217 decisions, no open questions |
| Compiler | **M1 reached**: parse → resolve → typecheck → MIR → interpreter / LLVM native (primitives, functions, loops, panics) |
| Toolchain | Rust 1.99.0 (rust-toolchain.toml), LLVM 23.1.3 (Homebrew `llvm@23`), inkwell 0.11 `llvm23-1` |
| Last green `scripts/test.sh` | 2026-11-01 (P5.5) |
| Host | macOS arm64 |

## Session protocol
1. Read this file, then the task in PLAN.md and its **Spec** sections. Nothing else unless you need it.
2. Implement it. Add golden tests (`tests/run`, `tests/fail`) or unit tests.
3. Run `scripts/test.sh`. Once it's green, commit as `P<x.y>: <summary>`.
4. Update this file: tick the box, add a log line, set **Next task**, and note any new
   decisions or known issues. Commit again (or amend into the same commit).
5. If a task is too large, split it in PLAN.md (`P3.7a`, `P3.7b`) and record that here.

## Known issues / notes for the next session
- **GC (P4.x)**: `runtime/src/ppl_gc.c` and `crates/ppl-codegen/src/gc.rs` (decisions 206–209).
  Generational (non-moving nursery + incremental old generation, Dijkstra barrier, cards for
  large objects, lazy sweeping). **Pause benchmark** `bench/gc_pause.ppl` (release build, macOS
  arm64, 200k-entity world updated 4000×/frame for 600 frames, 1.36 GB allocated, ~141 MiB heap):
  frame-loop **max pause 0.34–0.77 ms** over 5 runs, typical nursery pause ~0.2–0.4 ms, GC time
  ~2.5 ms/frame. The setup phase alone has one ~0.7–1 ms nursery pause (scanning a freshly grown
  1.6 MB array). `PPL_GC_TRACE=1` prints each pause.
  Weak refs (P4.4): `@weak` fields (core library only) → `PplLayout.nweak`; every nursery
  collection walks the whole list of objects with weak words (fine for now; a generational
  split of that list would help programs with very many weak refs). No ephemerons in `WeakMap`.
  Finalizers run at the end of the entry's code (`__runFinalizers`), to move onto the event
  loop in P5.1.
- **Unsafe (P4.5, decision 210)**: `ptr<T>` = `Ty::Ref`. An `unsafe function` used as a value
  (`const f = poke`) can then be called without `unsafe` (not checked yet). FFI (P? interop)
  will need `ptr<void>`, null pointers and casts between pointer types.
- **Workers (P5.5, decision 217)**: check threading changes with ThreadSanitizer:
  `ppl build --emit llvm-ir x.ppl > x.ll` then
  `/opt/homebrew/opt/llvm@23/bin/clang -g -O1 -fsanitize=thread -Iruntime/include x.ll runtime/src/*.c`.
  Every mutable C static must be `_Thread_local` (except the worker table). Thread-local
  access costs ~12% of GC time on `bench/gc_pause.ppl` (max pause unchanged).
- **Event loop (P5.1, decision 213)**: `stdlib/core/event.ppl` + `runtime/src/ppl_loop.c`.
  Timer tests use gaps of ≥ 10 ms (they run real time, also under `PPL_GC_STRESS`).
- **async (P5.2, decision 214)**: desugaring in resolve.rs (`desugar_async`, `lower_await`,
  `build` AST helpers); `Promise` in `stdlib/core/promise.ppl`. Node 24 is installed
  (`node`), handy for checking orderings. Not yet: async lambdas (`async () => …`), `await`
  of non-promises, promise `catch`/`finally` methods, throwables inside async code.
- **Arenas (P4.6, decision 212)**: Arena objects are scanned at every nursery collection (cost grows with
  open arena size). Exceptions leaving an arena are deep-copied out; other escapes panic
  natively only. Arenas inside generators and `async` code are untested.
- `ppl run` builds natively; `ppl run --interp` uses the interpreter. `// native-only:` in a
  run fixture skips the interpreter.
  Collection only at safepoints; layouts mirror codegen's `size_align` (wasm32's 4-byte
  pointers will need both updated in P8.5). The interpreter has no collector.
  `PPL_GC_STRESS=1` (also run by the golden harness) collects at every safepoint.
- **Decisions 197–205 (P3.19–P3.26) are provisional** too: exceptions via error-return lowering,
  throws inference/polymorphism, panic unwinding with setjmp guards, the core library in ppl
  (`stdlib/core`), `Iterator<T>` as a class (no generic interfaces yet), and module resolution.
- Modules: the resolver keys top-level names by module (`Names<T>`, `ModCtx` in resolve.rs); the
  driver's `modules.rs` loads files and strips import/export declarations. MIR `Loc`s have no
  file, so panics inside non-entry modules report `0:0` (and the entry's file name).
- Generic interfaces are the main missing piece for a real `Iterable<T>`/`Iterator<T>`.
- The core library lives in `stdlib/core/*.ppl` (`ppl_driver::PRELUDE`, `include_str!`). Built-in type methods are `__Array_x`/`__String_x` functions (`Program.extensions`, `Types.extensions`). `mir::live_functions` drops unreachable `library` functions in dumps and codegen. String building is O(n²) (concatenation); a string builder would help. `Map.keys()` etc. return arrays until P3.23 gives iterators.
- LLVM can't emit WasmGC, so the browser target starts on linear memory (PLAN P8.6 and P8.7).
- lld isn't installed (separate Homebrew `lld` formula); needed from P8.1.
- Exceptions use error-return lowering (decision 197): `Rvalue::SetException/TakeException/
  ExceptionPending`, `Terminator::Uncaught`; `FnBuilder::terminate` adds the pending check after
  calls of throwing functions; the throws analysis is `ppl-sema/src/throws.rs`. The prelude
  (`ppl_driver::PRELUDE`) is a second source file; lowering gives its spans no location.
  The pending slot is a plain global: make it thread-local with threads (P5).
- **Decisions 183+ are provisional** (made during an unattended session): pop → `T?`,
  array-literal inference, no `==` on `T[]`, range step rules, debug format of arrays, null
  representation and narrowing rules. Ask the author to confirm.
- Narrowing is variables-only; `p.x != null` doesn't narrow `p.x` (fields can change through
  aliases). `unify_all` (return/break joins) doesn't make `T` + `null` into `T?` yet; `if/else`
  does. Closures (P3.15) must make captured-and-assigned variables non-narrowable.
- A method called on an array element (`xs[0].bump()`) gets a raw pointer into the array's
  storage natively; if the method grows the same array, the pointer dangles. Revisit with the GC.
- `==` on large fixed arrays is unrolled per element in codegen.
- Enum methods (`enum Token { … name(): string { … } }`) report E0900; `this` is struct-only
  (`Function.this: (LocalId, StructId)`), so enums can't implement interfaces yet either.
- Interfaces are StructDefs with `is_interface` (`Ty::Iface`); itables are built at the end
  of lowering (`Cx::build_itables`), with MIR-only adapter functions appended to `fns`.
- §3.5's `switch (cmd) { case "build" … case Run { file } … }` could now use a union
  (`string | Run`); `tests/run/patterns.ppl` still splits it. `unknown` doesn't exist yet.
- Generic types: `Ty::type_owner(sid)` (high bit set) owns a type's parameters; methods and
  constructors of generic types lower only as instances (placeholders otherwise); MIR
  `Rvalue::New/Aggregate` carry the full type; codegen caches LLVM types per instantiation
  (`struct_llty`). Missing: generic enums/interfaces (no `Option<T>`/user `Comparable<T>`).
- Closures: resolver keeps a stack of enclosing `FnState`s (`outer_fns`) and captures on
  lookup (`capture`, `capture_this`); `Binding.boxed` marks heap-celled variables (MIR local
  `Ty::Ref(T)`, accessed through `Place::deref`). Closures in generic code are instantiated
  with the enclosing instance's substitution (`Cx.cur_subst`). `get`/`set` are lexer keywords
  — they should probably become contextual (they block `const get = …`).
- `ppl run` caches executables in $TMPDIR/ppl-cache; the key now includes the compiler binary
  (size + mtime). Before that fix, a rebuilt compiler could run a stale executable — if a native
  result ever looks impossible, `ppl build -o` and run that directly.
- Monomorphization: `Cx.types` is a `Cow<Types>`; an instance is lowered with
  `Types::substituted(fn, args)` (a full copy — fine for now, quadratic-ish for big programs).
  Instances are MIR functions `program.fns.len() + i` (before itable adapters).
- Lowering erases `Ty::Lits/Union/Variant` (`Ty::erase`, `erase_program`) at the end; inside
  lowering, typeck types are unerased, MIR place types become erased only afterwards
  (`mir::project` erases defensively).
- Classes reuse `StructDef` (`is_class`), `Ty::Class(StructId)`; a class method's `this` is
  the object reference (not `Ty::Ref`). Ctors are named `<Class>.constructor`; typeck runs
  definite assignment on those. Subclasses copy the base's fields first (same indices), so
  base methods work on subclass objects; `declared_in` tracks where inherited members come
  from (visibility). Virtual dispatch: `Callee::Virtual { slot, decl }`.
- `match` lowers to sequential arm tests (no decision tree yet); unreachable arms aren't warned
  about (the usefulness checker could do it: `exhaust::Checker::useful` per arm).

## Checklist

### Phase 1: Foundation
- [x] P1.1 Toolchain setup
- [x] P1.2 Workspace skeleton
- [x] P1.3 Diagnostics
- [x] P1.4 Lexer: core tokens
- [x] P1.5 Lexer: templates, multiline strings, regex
- [x] P1.6 Statement-termination rule
- [x] P1.7 AST and pretty-printer
- [x] P1.8 Parser: expressions
- [x] P1.9 Parser: statements and blocks
- [x] P1.10 Parser: declarations and types
- [x] P1.11 Parser: patterns, match, switch, try
- [x] P1.12 Parser error recovery
- [x] P1.13 Golden-test harness

### Phase 2: First executable
- [x] P2.1 Name resolution
- [x] P2.2 Type checker: primitives and functions
- [x] P2.3 MIR design and lowering
- [x] P2.4 MIR interpreter
- [x] P2.5 C runtime v0
- [x] P2.6 LLVM codegen v0
- [x] P2.7 `ppl build` / `ppl run` end to end (**M1: hello world**)

### Phase 3: Core language
- [x] P3.1 Allocator v0 and string runtime
- [x] P3.2 char type and template literals
- [x] P3.3 Format specs
- [x] P3.4 Structs
- [x] P3.5 Arrays, fixed arrays, tuples, ranges
- [x] P3.6 Null and narrowing infrastructure
- [x] P3.7 Enums and basic match
- [x] P3.8 Advanced patterns and switch
- [x] P3.9 Classes I
- [x] P3.10 Classes II: inheritance
- [x] P3.11 Interfaces
- [x] P3.12 Unions and literal types
- [x] P3.13 Generics I: functions
- [x] P3.14 Generics II: types and where
- [x] P3.15 Closures
- [x] P3.16 Function features
- [x] P3.17 Operators and equality protocols
- [x] P3.18 Distinct types, aliases, immutability
- [x] P3.19 Exceptions I
- [x] P3.20 Exceptions II
- [x] P3.20b Lightweight throwables (split from P3.20)
- [x] P3.21 Panics and stack traces
- [x] P3.22 Core collections and Array API
- [x] P3.23 Iterators and generators
- [x] P3.24 Modules
- [x] P3.25 Destructuring and spread
- [x] P3.26 `using` and Disposable

### Phase 4: Memory management
- [x] P4.1 Shadow stack and type maps
- [x] P4.2 Mark-sweep GC
- [x] P4.3 Generational and incremental GC
- [x] P4.4 Weak references and finalization
- [x] P4.5 Unsafe and pointers
- [x] P4.6 Arenas

### Phase 5: Async and concurrency
- [x] P5.1 Event loop
- [x] P5.2 async/await
- [x] P5.3 Async generators and `await using`
- [x] P5.4 Cancellation
- [x] P5.5 Workers
- [ ] P5.6 Transferables, inline workers, pools

### Phase 6: Comptime and metaprogramming
- [ ] P6.1 Constant folding
- [ ] P6.2 `comptime` evaluation
- [ ] P6.3 Types as comptime values
- [ ] P6.4 Decorators and std/meta
- [ ] P6.5 Quasi-quote templates and the AST builder
- [ ] P6.6 `@derive` in ppl
- [ ] P6.7 Runtime reflection
- [ ] P6.8 JSON

### Phase 7: Standard library
- [ ] P7.1 std/fs and std/path
- [ ] P7.2 std/process
- [ ] P7.3 std/net
- [ ] P7.4 Web Streams and encoding
- [ ] P7.5 fetch and std/http
- [ ] P7.6 Regex
- [ ] P7.7 Time
- [ ] P7.8 Crypto
- [ ] P7.9 Math, collections, fmt
- [ ] P7.10 std/test and `ppl test`

### Phase 8: Targets
- [ ] P8.1 Cross-compilation infrastructure
- [ ] P8.2 Linux gnu and musl
- [ ] P8.3 Windows (MinGW)
- [ ] P8.4 macOS cross builds
- [ ] P8.5 wasm32-wasi
- [ ] P8.6 wasm32-browser (linear memory)
- [ ] P8.7 WasmGC backend
- [ ] P8.8 JS imports from .d.ts
- [ ] P8.9 Platform files and conditionals

### Phase 9: C interop
- [ ] P9.1 extern "C"
- [ ] P9.2 C header import
- [ ] P9.3 Export to C

### Phase 10: Tooling and ecosystem
- [ ] P10.1 Formatter
- [ ] P10.2 Lint
- [ ] P10.3 Package manager
- [ ] P10.4 Registry
- [ ] P10.5 Incremental compilation
- [ ] P10.6 LSP and VS Code extension
- [ ] P10.7 REPL and JIT
- [ ] P10.8 Debugging
- [ ] P10.9 Documentation tooling
- [ ] P10.10 Release packaging

### Phase 11: Later
- [ ] P11.1 `--no-gc` subset + bare metal
- [ ] P11.2 riscv64
- [ ] P11.3 Android
- [ ] P11.4 iOS
- [ ] P11.5 Self-hosting investigation

## Log
| Date | Task | Notes |
|------|------|-------|
| 2026-10-31 | Spec | Design interview completed. Spec chapters 00–15 and DECISIONS.md #1–162 written. |
| 2026-10-31 | Plan | PLAN.md split into 103 session-sized tasks, and PROGRESS.md created. |
| 2026-10-31 | Setup | CLAUDE.md added (session start, auto-commit rules). |
| 2026-10-31 | P1.1 | Rust 1.99.0 + LLVM 23.1.3 were already installed (keg-only). Pinned toolchain, added scripts/env.sh and docs/DEVELOPMENT.md. Used LLVM 23 instead of 18 (inkwell 0.11 supports it); inkwell smoke test OK. |
| 2026-10-31 | Tour | docs/TOUR.md: an example of every feature. Open questions for the user: `Some(v)` in §3.5 (no Option type), undeclared `cache` in the §11.2 `@memoize` example, `env: { "LANG": "C" }` object literal (§13.3), option bags vs named args for Web APIs, the WasmGC wording in §9.2 vs PLAN. |
| 2026-10-31 | P1.2 | Cargo workspace (edition 2024, resolver 3) with 10 crates, `ppl` binary (clap) with `--version` + integration test, scripts/test.sh (fmt, clippy -D warnings, test). |
| 2026-10-31 | P1.3 | ppl-diag: SourceMap/FileId/Span, Diagnostic builder, error-code registry (decision 163), plain renderer matching §14.2 (snapshots). Fixed the spec example's column (4:20 → 4:19). |
| 2026-10-31 | P1.4 | Lexer (with P1.5): tokens, keywords (decision 164: `do` added, contextual `type`/`of`/`from`/`arena`), numbers with suffixes, strings/escapes, comments, newline tokens. |
| 2026-10-31 | P1.5 | Templates via a substitution stack, format-spec capture at a top-level `:` (ternary-aware), triple-quoted dedent, regex literals (decision 165). Committed together with P1.4. |
| 2026-10-31 | P1.6 | newline::filter with bracket stack; trailing/leading continuation sets (decision 166, provisional: generic closers and `?` end a line); table-driven tests for every §1.2 example. |
| 2026-10-31 | P1.7 | ast.rs covers the whole language (exprs incl. control flow as expressions, patterns, stmts, decls, members); dump.rs S-expression printer with width-based breaking; unit tests on hand-built ASTs. `ppl parse --dump` gets wired in P1.8. |
| 2026-10-31 | P1.8–P1.11 | Parser (one module, committed together): Pratt expressions, statements, declarations, types, patterns, match/switch/try. `ppl parse --dump`. 14 insta snapshot groups; `tests/spec_samples.rs` parses every ```ts block in docs/spec and docs/TOUR.md (2 spec blocks skipped with reasons). Decisions 167–172. |
| 2026-10-31 | P1.12 | Recovery: statement-level sync (no separator error after a failed statement, stops at statement keywords on a new line), bracket-depth-safe member/arm recovery, unclosed `(`/`[` end before a declaration keyword (newline filter), enum variant/method lookahead. Targeted messages: `var`, `for-in`, `===`, conditions without parens, trailing-operator line continuation. 6 multi-error fixtures in tests/fail/parse with `// error:` directives + snapshots. |
| 2026-10-31 | Review | Author answered all pending questions. Confirmed 166–168, 170, 172 (now in spec §1.2/§1.3/§2.8/§3.2/§3.3/§3.5); new decisions 173–178: no Option type, decorator hidden statics (`addStatic`), Map literals, WasmGC interim note, `Iterable.iterator()`, Web-standard options objects. Parser: parenthesized patterns (`case (s: string):`). Spec and TOUR examples updated. |
| 2026-10-31 | P1.13 | `ppl-tests`: discovery (multi-file dirs with main.ppl), directive parser (expect/exit/panic/error/at, malformed directives fail), LCS diff, pass/pending/fail report. `ppl-driver` got `check_source`/`check_file` and `Stage` (code ranges to stages); the harness marks checks for unbuilt stages as pending. Fixtures: run/{hello,fib,loops,overflow_panic}, fail/types/nominal. Broke two fixtures on purpose (wrong code + position; non-compiling run test), checked the report was readable, restored them. **Phase 1 complete.** |
| 2026-10-31 | P2.1 | ppl-sema: Ty, HIR (ExprIds, resolved Vars, desugared for/for-of-range/do-while), resolver: block scopes with TDZ-style use-before-declaration, order-independent top-level functions, entry globals, built-ins, const assignment, break/continue placement, did-you-mean suggestions. E02xx codes; E0900 'not implemented' makes golden fixtures pending. Driver sorts diagnostics by position. 6 fail fixtures in tests/fail/names. |
| 2026-10-31 | P2.2 | typeck.rs: bidirectional checking with literal typing from context (range-checked, incl. negated literals), lossless widening recorded as coercions, operator typing, if/blocks/loop-break values, never/void, if-without-else → T?, char inference from 1-char literals, `as` rules, on-demand return-type inference with recursion detection (E0306), built-in signatures. 6 fail fixtures in tests/fail/types. |
| 2026-10-31 | P2.3 | ppl-mir: CFG of basic blocks over typed locals; statements carry Locs (integer ops trap implicitly); terminators goto/branch/return/call/panic/unreachable; &&/|| and compound &&= lowered to control flow; dead code after divergence goes to unreachable blocks. Lowering reports E0900 for strings/templates/nullable values for now. `ppl build --emit mir`; snapshots in ppl-driver/tests/mir.rs (loops, if expressions, loop/break value). |
| 2026-10-31 | P2.4 | ppl-interp: Value/Frame interpreter over MIR; integer ops trap (overflow, div0, shift range, negative exponent, casts) with shared messages in ppl_mir::mir::panics; f32 computed at f32 precision; JS-style float printing (format_float, unit-tested); depth limit → stack overflow panic; runs on a 1 GiB-stack thread (driver::run_interp). `ppl run --interp`; panic report 'panic: msg\\n  at file:line:col', exit 101. Harness checks stdout/exit/panic. 10 new run fixtures. |
| 2026-10-31 | P2.5 | runtime/ (C11): ppl_rt.h + ppl_rt.c with print functions, JS-style number formatting, ppl_panic (format + exit 101), main → ppl_entry. ppl-runtime builds libppl_rt.a via cc (no link directives into the compiler), macOS min 13.0 (decision 180/179). Tests link C programs: panic format, and C vs interpreter float formatting on ~6000 values — found Rust's {:e} picks the odd digit on ties; switched the interpreter to the same shortest-correctly-rounded loop (ties to even, as ECMAScript). |
| 2026-10-31 | P2.6 | ppl-codegen (inkwell 0.11/LLVM 23): allocas per MIR local, overflow intrinsics → cold noreturn ppl_panic, div0/MIN÷-1/shift/cast checks (casts range-checked in i128; float→int with exact power-of-two bounds — same bug fixed in the interpreter, new fixture), integer ** via runtime ppl_ipow_s/u, strings as PplStr* (runtime header gained PplStr, ppl_print_string, ppl_panic_str), internal `ppl.<name>` functions + external ppl_entry, object emission for the host. `ppl build --emit llvm-ir`; IR snapshots in ppl-driver/tests/llvm_ir.rs. |
| 2026-10-31 | P2.7 | Driver: build_native (LLVM object + cc link with libppl_rt.a), content-hashed run cache in $TMPDIR/ppl-cache. CLI: `ppl build [-o] [--release] [--emit mir|llvm-ir]`, `ppl run [--interp] [--release] [-- args]`. Harness runs every tests/run fixture interpreted AND native against the same directives: 31 pass. --release (O2) output matches the interpreter. **Milestone M1: hello world native.** |
| 2026-10-31 | P3.1 | Runtime: ppl_alloc (malloc-backed, never frees, aborts on OOM), PplStr ops ppl_str_new/concat/eq/cmp/char_at (cached position → sequential s[i] O(1); ASCII fast path). MIR Rvalue::StrLen/StrCharAt; string +, +=, ==, <… in interpreter and codegen (codegen does the bounds check so the message stays in ppl_mir::panics). Fixtures: strings (Polish diacritics, emoji), string_index_panic, string_scan (10k chars). Fixed: if-without-else in statement position no longer flagged as a nullable value. |
| 2026-10-31 | P3.2 | Templates lower to concatenation of quasis and Rvalue::ToStr(part) (runtime ppl_str_from_{i64,u64,f64,f32,bool,char}, interpreter shares display()); `x.toString()` on primitives; triple-quoted and backtick multiline literals verified end to end. Fixtures: run/templates (nesting, $${}, multiline dedent), run/chars, fail/types/char_literal (`const c: char = "ab"`). loops.ppl no longer pending. |
| 2026-10-31 | P3.3 | ppl_sema::format: spec parser + type check (E0310); Types.formats; MIR Rvalue::Format; interpreter format_spec and C ppl_fmt_{i64,u64,f64,str,char,bool} with a constant PplFmt global from codegen; shortest_digits shared by e/f/%. Fixtures: run/format_specs (all §1.4 examples except `point:?`, every tour example, 40 cases, identical natively), fail/types/format_specs. Decision 181 + spec §1.4 rules. Note: a runtime edit briefly corrupted ppl_rt.c (empty-slice replace); restored from git before anything was committed. |
| 2026-10-31 | P3.4 | Structs end to end: Ty::Struct (Arc name), HIR StructDef/Target/Object, resolver (pass 1 structs, fields+methods, by-value cycle check E0210, methods with hidden `this`, implicit members, field targets), typeck (field indices, struct literals with E0311/E0312/E0313, method calls via shared call_fn, field-wise ==, `?` only), MIR places with projections + Deref base, Aggregate/Ref rvalues, StructLayouts; interpreter locals as Rc<RefCell> cells with Value::Ref; codegen named LLVM struct types, GEP places, insertvalue aggregates, struct_eq, generated ppl.debug.<Name> helpers. Decision 182 (asked): const fixes only the binding. Fixtures: run/structs (copy semantics, mutation via methods, nesting, ==, debug), fail/types/structs, fail/names/recursive_struct; `${point:?}` added to format_specs. |
| 2026-11-01 | P3.5 | Ty::Array/Fixed/Tuple/Range; MIR `Proj::{Field, Index(local)}` with bounds checks emitted by lowering (after an assignment's value, so the value can't invalidate them); rvalues Tuple/Repeat/ArrayNew/ArrayLen/ArrayPush/ArrayPop/InBounds/AddOverflows; HIR ForOf (ranges, arrays, fixed arrays, strings) and let/for-of destructuring of array patterns (incl. top level); runtime PplArray (ppl_array_new/push/pop); interpreter paths through shared arrays (`Step::Part/Elem`); generalized `${v:?}` debug helpers. E0314–E0316. Decision 183 (provisional). Fixtures: run/{arrays,tuples_fixed,ranges,array_index_panic,range_step_panic}, fail/types/arrays. |
| 2026-11-01 | P3.6 | `T?` as nullable pointer (refs) or `{i1, T}`; MIR Const::Null, Proj::Payload, WrapSome/IsNull/Unwrap; convert() handles T→T?, T?→U?, null; `??`, `??=`, `!` (panic), `?.` chains (resolver rewrites the outermost link onto a hidden local: HIR OptChain); `x == null` lowers to IsNull; if-without-else values; printing/debug of nulls; pop() returns T?. Typeck narrowing scopes with facts from conditions, `&&`/`\|\|`, early exits; assignments/loops forget. Null-safe ppl_str_eq; entry-block allocas for debug/pow temporaries. Decision 184. Fixtures: run/{nullable,null_unwrap_panic}, fail/types/nullable. |
| 2026-11-01 | P3.7 | Ty::Enum, HIR EnumDef/VariantDef, patterns (Wild/Bind/Lit/Variant positional+named/Struct) and Arm with guard; resolver: `Enum.Variant` construction (named + positional args), variant-name identifier patterns, enum cycle check generalized; typeck: constructor arg mapping (E0311/E0312/E0103), pattern checking (E0318), Maranget usefulness with witnesses (`exhaust.rs`, E0317 lists missing cases), unify_exprs for arms; MIR Proj::VariantField, Rvalue::Variant/Tag, sequential match lowering; interpreter Value::Enum; codegen `{i32, [k x i64]}` enums with own size/align computation, generated ppl.eq.<Enum> and debug helpers. Decision 185. Fixtures: run/enums, fail/types/match, fail/names/enums. |
| 2026-11-01 | P3.8 | HIR PatKind Or/Range/Slice/As/Type + `Pattern::looks_inside`; ExprKind::Is and Match.switch. Resolver: or-alternatives share locals (Binds with reuse), type names as type-test patterns, condition-scoped `is` bindings (`if`/`while`, through `&&`/parens), switch statements vs values. Typeck: bind_local (or-alternatives agree), slice/type/range checks, `is` facts for narrowing, switch statements skip exhaustiveness. Exhaustiveness: or-expansion (capped product), Ctor::Slice with length constructors (`[_, _, ...]` witnesses), fixed arrays. Lowering: single match_pattern (test + bind), or via shared success block, ranges, slices (Proj::Index with const locals, ArraySlice for rest). Runtime ppl_array_slice. Decision 186. Fixtures: run/patterns (every §3.5 example), fail/types/patterns, fail/names/or_patterns. |
| 2026-11-01 | P3.9 | Classes: StructDef gains is_class/ctor/getters/setters/statics/private set, Function.owner; resolver class_members (fields with literal-typed inits, param properties, accessors, static fields as globals + static init functions called first in the entry, static methods), synthesized constructors with a prologue, `new`, `Class.static` access, private static checks; typeck visibility (E0319) for fields/methods/accessors, getters/setters (incl. compound assignment), identity `==`, definite assignment walk (E0320), static inits checked first; MIR Rvalue::New, class field projections through the reference; interpreter Value::Object (Rc<Obj>) and Step::ObjField; codegen `{ptr header, fields}` objects via ppl_alloc_zeroed. Fixed: assignment checks its value before ending the target's narrowing (`cur = cur.next`). Decision 187. Fixtures: run/classes, fail/types/classes, fail/names/classes; fail/types/nominal now runs. |
| 2026-11-01 | P3.10 | Inheritance: classes ordered base-first (cycle/unknown/final-base errors), `inherit` copies fields/methods/accessors/vtable/visibility; vtables with required `override` (E0321), final-by-default and `final` (E0322), abstract stubs + E0323/E0324; constructor overloads by arity, `super(…)`/`this(…)` as HIR CtorCall (first statement only, E0326), forwarding default constructors, `super.m()` (SuperCall), `instanceof` + narrowing, upcasts (typeck `upcasts`, no-op conversions), protected access, override signature check (E0325), downcasting type/class patterns (`downcasts`, InstanceOf test into a subclass-typed temp). MIR ClassMeta (base, vtable), Callee::Virtual, Rvalue::InstanceOf; interpreter dispatch by object class; codegen `ppl.classinfo.<C>` globals `{base, [n x ptr]}`, header store, indirect calls, runtime ppl_instance_of. Fixed: a diverging body in a value-returning function ends in `unreachable`. Decision 188. Fixtures: run/inheritance, fail/names/{inheritance,ctor_call}, fail/types/inheritance. |
| 2026-11-01 | P3.11 | Interfaces: Ty::Iface (StructDef.is_interface, `implements` lists), resolver orders interfaces by `extends`, inherited-first method lists with defaults/abstract stubs, conformance (E0327 missing, E0328 conflicting defaults, public implementations); typeck nominal upcasts to interfaces (class/struct, first-parent interface chains), interface call slots, implementation signature checks (E0325). Lowering: ToIface (boxing structs), IfaceData, Callee::Iface, itables + adapters (virtual when needed). Interpreter Value::Iface; codegen `{ptr, ptr}` fat pointers and `ppl.itable.N` globals. Decision 189. Fixtures: run/interfaces, fail/types/interfaces (structural match without `implements`), fail/names/interfaces. |
| 2026-11-01 | P3.12 | Ty::Lits/Union/Variant + LitVal; type aliases (lazy, cycle check); resolver union normalization (flatten, null → `T?`, literal merging), interned anonymous enums for unions (EnumOrigin::Union), discriminated object unions (EnumOrigin::Discriminated, discriminant = field 0), `typeof` (HIR TypeOf). Typeck: literals into literal types/union members (`lit_in`), union coercions and joins, discriminant member access and `e.kind == …` / `typeof x == …` narrowing (`tag_facts`), discriminated object literals, union patterns (`union_tests`), exhaustiveness for literal unions and union members. Lowering: Variant construction into unions, type names by tag, union stringification, narrowed reads into members, `erase_program`. Decision 190. Fixtures: run/unions (every §2.5/§2.6 example), fail/types/unions. |
| 2026-11-01 | P3.13 | Generic functions: Ty::Param(fn, index, name) + subst/has_params; HIR TypeParam/Bound (Comparable, Equatable, interfaces; `extends`, `where`, `&`), explicit type args on calls; typeck call_generic (3-pass inference + unify, E0329/E0330), operations on parameters via bounds (`compareTo`/`equals`, comparisons, interface methods → param_calls), Types::substituted; lowering worklist of instances (`f<i64>`), placeholder for generic bodies, param_call per concrete type (primitive compare → -1/0/1, struct/class/interface methods). MIR snapshot `generic_instances`. Decision 191. Fixtures: run/generics (max<T> over ints, floats, strings, chars), fail/types/generics. |
| 2026-11-01 | P3.14 | Generic structs/classes: type args in Ty::Struct/Class/Iface (mechanical change), StructDef.generics with defaults, identity instantiation for `this`; resolver `Name<…>` in types/`new`/literals, generic methods, global annotations deferred until types are declared; typeck field types substituted by receiver args, call_inferring (shared by generic fns, methods of generic types, generic constructors via `new` with expected-type args), struct literal inference, unify through type args; lowering multi-owner substitutions (`Stack<i64>.push`, `Box<i64>.map<string>`), method_target; MIR New/Aggregate(Ty), projections substitute; codegen per-instantiation LLVM types. Decision 192. Fixtures: run/generic_types (Stack<T>, Pair<A, B>, defaults, generic method, Node<T> list), fail/names/generic_types, fail/types/generic_types. |
| 2026-11-01 | P3.15 | Closures: Ty::Fn and `(x: T) => U` types; resolver lifts arrows/function expressions (ExprKind::Closure, Function.closure with captures), capture lookup through enclosing functions (by value for const, boxing captured lets/params), arrow `this` capture, per-iteration rebox for captured C-style `for (let …)` variables; typeck closure checking with contextual parameter types, named functions as values, calls through function values; MIR Rvalue::Closure/BoxNew, Callee::Indirect, env as first parameter, boxed locals via Ref/deref, function-value adapters; interpreter Value::Closure; codegen `{fnptr, env}` with heap environments and indirect calls. Entry function id is now reserved before top-level resolution. Decision 193. Fixtures: run/closures (§5.5 counter, loop-variable closures, makeAdder, shared lets, method arrows), fail/types/closures. |
| 2026-11-01 | P3.16 | Named arguments, defaults (callee-evaluated with hidden flags; may use earlier params), optional `?` params, rest params and `...array`/`...tuple` spreads via typeck `Slot`s (`map_args`/`check_slots`) and lowering `call_operands`; overload sets for top-level functions with applicability + specificity resolution (E0332/E0333, same-signature duplicates still E0203); `tailcall` (E0331) as MIR Terminator::TailCall → LLVM musttail / interpreter frame reuse (1,000,000-deep recursion runs). Const::Undef for omitted arguments. Decision 194. Fixtures: run/function_features, fail/types/function_features. |
| 2026-11-01 | P3.17 | Operators: `operator` members as methods `op+`/`op-u`/`op[]`/`op[]=` (resolver), typeck user_binary/unary/index/index-assign/compound dispatch (op_calls), `==` → user `equals`, comparisons → user or derived `compareTo` (derived), derived `equals`/`compareTo`/`hashCode` method calls; `@derive(Eq, Hash, Ord)` on classes; lowering user_call/receiver, compare_values/cmp_into (structs, tuples, fixed, ranges, enums, nullables, classes), eq_values (derived class equality), hash_value/hash_parts; MIR Rvalue::Hash/HashCombine with matching interpreter and C runtime (ppl_hash_u64/str/combine); bool `&`/`\|`/`^`. Fixed: run cache keyed on the compiler binary. Decision 195. Fixtures: run/operators (Vec2 arithmetic, sorting structs and classes by compareTo, derive), fail/types/operators; bound fixtures now use classes. |
| 2026-11-01 | P3.18 | Ty::Distinct (from `type X = distinct T`, erased to the base; `as` conversions; literals adopt numeric distinct types), Ty::Immutable (typeck-only, stripped from Types before lowering): deep views with pure-value normalization, `through_immutable` checks for field/element assignment and non-readonly method calls (incl. push/pop), `readonly` methods (immutable `this`), `readonly` fields (E0334) incl. parameter properties; struct members accept `readonly`. Lowering erases literal types. Decision 196. Fixtures: run/distinct, fail/types/immutability (mutation through `immutable`, `Meters`/`Feet` mixing). Type aliases were done in P3.12. |
| 2026-11-01 | P3.19 | Error-return exceptions: prelude `Error` class, `throw`, `try`/`catch`/`finally` as expression (and statement), typed/pattern catch clauses, `throws` on functions, methods and constructors; checked-throws pass (E0336) with catch narrowing; non-`Error` throws E0337; top-level uncaught → `uncaught Class: msg`, exit 1 (interp `Panic.uncaught`, runtime `ppl_uncaught`). `finally` runs on return/break/continue. Decision 197. Fixtures: run/errors/exceptions, fail/types/{exceptions,throw}, fail/names/throws_clause. IR/MIR snapshots now include the prelude. |
| 2026-11-01 | P3.20 | Throws inference (lambdas, top-level fns; `Types.fn_throws`, fixpoint in `throws::check`), `Ty::Fn` carries throws (resolved `throws` in fn types, display, subclass-aware upcast), `E extends Error` (`Bound::Error`) bound from callbacks in `unify`, lambdas deferred to the last inference pass, `try?`/`try!`, checks after every indirect call. Lightweight throwables split into P3.20b. Decision 198. Fixtures: run/errors/{exceptions_inference,try_bang_panic}, fail/types/exceptions (rewritten: methods declare, top-level infers; map/parseInt precision). |
| 2026-11-01 | P3.21 | Panic unwinding through `finally`: MIR `Guard`/`Unguard`/`Resume` terminators (try-with-finally), interpreter handler stacks, native `_setjmp` + runtime guard stack (`ppl_guard_push/pop`, `ppl_resume`), `optnone` for guarded functions. Stack traces: interpreter call-name stack; native `backtrace` + `dladdr` (addr-1), frame pointers everywhere, debug builds export `ppl.*` symbols. Decision 199. Tests: run/errors/panic_finally, driver panic_trace snapshots (native macOS and interpreter, identical). |
| 2026-11-01 | P3.22 | Core library in ppl (`stdlib/core`: error, array, string, map/set) loaded as prelude files; extension methods on arrays/strings via `__Array_*`/`__String_*` functions (generic, throws-polymorphic); `Hashable<T>` bound and `hashCode()` on type parameters; type params in templates; getters/setters of generic class instances; `StrSlice` primitive (`ppl_str_slice`); live-function analysis (library code only when reached). Decision 200. Tests: run/stdlib/{collections,throwing_callbacks}, fail/types/stdlib_throws; IR/MIR snapshots shrank. |
| 2026-11-01 | P3.23 | `Iterator<T>` core class (step closure) with lazy adapters; `function*`/`*method` desugared in the resolver to `new Iterator(step)`, the step closure lowered as a MIR state machine (frame in the closure environment, `Cx::gen_frames`, dispatch/save blocks filled after the body); `for-of` over iterators and `iterator()` methods (`Types.iterables`, `for_of_iterator`); `Map`/`Set`/array iterators; null comparisons always allowed; T+null joins; lambda result hints. Decision 201. Tests: run/iterators/generators (fib().take(10) etc.), fail/types/generators, fail/names/yield. |
| 2026-11-01 | P3.24 | Modules: driver loader (`modules.rs`: relative/extensionless/`index.ppl`/`std/` resolution, DFS init order, export/import tables, `export const` unwrapping), resolver namespaces per module (`Names<T>`, `ModCtx`, `resolve_modules`), `* as` namespaces for values and types, re-exports, E0338 library `let`, E0339 init cycles, E0340 not exported, E0341 unknown module; harness resolves imports from each fixture's directory; `stdlib/greeting.ppl` as a first `std/` module. Decision 202. Fixtures: run/modules/{basic,cycle}, fail/modules/{init_cycle,library_let,exports,missing}. |
| 2026-11-01 | P3.20b | Lightweight throwables: prelude `Throwable` interface, throwable enums (`EnumDef.throwable`), `Program::throwables`/`throw_tag`, boxed values with a tag slot (`SetException(v, tag)`, `ExceptionTag`; interp field, native `ppl.exception_tag` global), catch dispatch by tag (`catch_pattern`), shared `exception_text` for uncaught/`try!`, throws analysis `Catch` kinds. Decision 203. Tests: run/errors/throwables, fail/types/{throwables,throw_value}. |
| 2026-11-01 | P3.25 | Destructuring desugared in the resolver: object patterns with defaults, array `...rest` (slice), destructured parameters (`split_destructured`, `destructured_params`), destructuring assignment (`assign_destructured`), array spread via `concat`/`slice`. Decision 204 (object `...rest` still missing). Tests: run/destructuring (§4.1, §5.3, §5.7 examples), fail/{names,types}/destructuring. |
| 2026-11-01 | P3.26 | `using` desugared in `block_stmts` to `const` + `try { rest } finally { x?.dispose() }` (reverse order, exceptions, early exits, panics); prelude `Disposable`. Decision 205. Tests: run/using, fail/{types,names}/using. **Phase 3 complete.** |
| 2026-11-01 | P4.1 | GC roots: `PplLayout` type maps (codegen `gc.rs`: reference offsets mirroring `size_align`, enum payloads conservative), shadow-stack frames (traced locals in one zeroed frame struct, pushed at entry, popped at return/tailcall, restored by panic guards), global-roots table, safepoints at entry and loop headers (`ppl_gc_requested`), write-barrier calls on heap stores, `ppl_gc_alloc(size, layout)` everywhere (still malloc), `PplArray.elem`; `__gc_*` builtins, `std/gc` (`gc`, `GcStats`); interpreter frame stack and `dumpRoots`. Decision 206. Test: run/gc/roots. |
| 2026-11-01 | P4.2 | Mark-sweep heap in `ppl_gc.c`: 64 KiB blocks, 27 size classes (bump then free lists), large-object block runs, block hash table for pointer validation (interior pointers), mark stack, sweep with spare-block reuse, threshold 2× survivors (≥ 8 MiB), pause/time stats, `pause`/`resume`; `PPL_GC_STRESS=1` and a third harness run per fixture. Decision 207. Test: run/gc/stress (200k iterations: 38 MB allocated in an 8 MB heap, 5 collections, max pause 0.46 ms). |
| 2026-11-01 | P4.3 | Generational + incremental GC: nursery of young objects (list-swept, promoted in place), remembered set + cards for large objects, write barrier with layouts (Dijkstra shading during marking), incremental marking with alternating colors and chunked large objects, finishing pause (nursery + root rescan + drain), lazy sweeping, radix block table; `collectYoung`, `pause`/`resume`, `resetStats`; `PPL_GC_STRESS` overlaps cycles with the mutator, `PPL_GC_TRACE`. Decision 208. Tests: run/gc/{generations,control}; bench/gc_pause.ppl: frame max pause 0.34–0.77 ms (see notes). |
| 2026-11-01 | P4.4 | `WeakRef`, `WeakMap`, `WeakSet`, `FinalizationRegistry` in `stdlib/core/weak.ppl` over a core-only `@weak` field (`StructLayout.weak`, `PplLayout.nweak`; the collector clears dead targets after nursery marking and at the end of major marking), `T extends object` bound, `__ref_id` builtin; interpreter weak fields as `rc::Weak`; finalizers at exit through `__runFinalizers` (dropped when unused). Decision 209. Tests: run/gc/{weak,finalization}, fail/gc/weak_targets. |
| 2026-11-01 | P4.5 | `unsafe { }`, `unsafe function`, `ptr<T>` (as `Ty::Ref`), `ptr.of`, `read`/`write`, pointer `+`/`-`/`==`, `sizeof`/`alignof` (`mir::Program::size_align`, moved from codegen), `unsafe.cast` bit casts, `@pod` structs; E0342 (needs `unsafe`), E0343 (non-plain `@pod` field). Decision 210. Tests: run/unsafe/pointers, fail/unsafe/{outside,pod}. |
| 2026-11-01 | P4.6 | Scoped `arena { }` (runtime arena chunks in the block table, roots for the GC, barrier + exit escape checks, exception evacuation, guard restore; lowering via the `finally` exit list; static escape checks E0344), `Arena` class (raw chunks), explicit method type arguments and named arguments for generic calls, `parseInt`, `int[].sum()`, harness `// native-only:`. Decisions 211–212. Tests: run/arena/{scoped,raw,escape_panic,escape_frame}, fail/arena/escape. Phase 4 done. |
| 2026-11-01 | P5.1 | Event loop in the core library (`event.ppl`: timers, intervals, microtasks, JS ordering; finalizers when idle), `__clock_ms`/`__loop_wait` builtins over `ppl_loop.c` (kqueue/epoll), exit hook `__runEventLoop` (dropped when unused). Decision 213. Test: run/async/timers. |
| 2026-11-01 | P5.2 | async functions/methods desugared to generator steps over `Promise<T, E>` (core library: executor ctor, `then`, `resolve`/`reject`/`all`/`race`/`allSettled`), checked rethrow, top-level `await`, unhandled rejections (exit 1), E0345; omitted trailing `void` args, `void`-expected lambdas. Decision 214. Tests: run/async/{ordering (= Node), basics, unhandled_rejection}, fail/async/{unhandled, await_outside}. |
| 2026-11-01 | P5.3 | `async function*` over a core `AsyncIterator<T, E>`, `for await`, `await using` + `AsyncDisposable`; async results held until the step finishes; panic guards re-armed after `yield`; `x != null` for any type. Decision 215. Tests: run/async/{generators (§5.7 example), await_using (= Node)}, run/errors/generator_finally_panic. |
| 2026-11-01 | P5.4 | `AbortController`/`AbortSignal`/`AbortError` (core), `std/timers` `delay(ms, signal?)` rejecting with `AbortError`. Decision 216. Test: run/async/abort. |
| 2026-11-01 | P5.5 | Workers: thread-local runtime/program state, worker modules as extra roots with their own entry functions, `ppl_worker.c` (threads, queues, wakers, generic deep copy), `Worker`/`WorkerPanicked`/`std/worker`, `Sendable` bound, interpreter threads. Decision 217. Tests: run/workers/{pingpong, errors, graphs}, fail/workers/not_sendable. |
