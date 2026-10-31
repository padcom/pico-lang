# Progress

> **Starting a new session?** CLAUDE.md already tells the agent what to do, so *"continue"* is
> enough. Or say it explicitly: *"Do the next task from PROGRESS.md."*

## Next task
**P2.6: LLVM codegen v0** (see [PLAN.md](PLAN.md#p26-llvm-codegen-v0))

Notes:
- Golden fixtures: `tests/run/**/*.ppl` and `tests/fail/**/*.ppl`, run by `cargo test -p ppl-tests`
  (`PPL_TEST_FILTER=name` for a subset). Directives are documented in `crates/ppl-tests/src/lib.rs`.
- When a stage lands, add it to `ppl_driver::Stage::IMPLEMENTED` and call it from
  `ppl_driver::check_source`. Pending fixtures for that stage then start being checked for real.
  `tests/fail/types/nominal.ppl` (E0308) is waiting for the type checker.
- New error codes go in `crates/ppl-diag/src/codes.rs` (E02xx for resolution).
- Commit only through a `scripts/test.sh` exit-status check.
- No open spec questions. Decision 178 matters for stdlib work: Web-standard APIs take their
  options object as a struct (`fetch(url, { … })`); ppl's own APIs use named arguments.

## Current state
| Area | State |
|------|-------|
| Spec | ✅ 16 chapters, 178 decisions, no open questions |
| Compiler | Phase 1 done: lexer, newline filter, AST, parser (`ppl parse --dump`), golden harness |
| Toolchain | Rust 1.99.0 (rust-toolchain.toml), LLVM 23.1.3 (Homebrew `llvm@23`), inkwell 0.11 `llvm23-1` |
| Last green `scripts/test.sh` | 2026-10-31 (P2.5) |
| Host | macOS arm64 |

## Session protocol
1. Read this file, then the task in PLAN.md and its **Spec** sections. Nothing else unless you need it.
2. Implement it. Add golden tests (`tests/run`, `tests/fail`) or unit tests.
3. Run `scripts/test.sh`. Once it's green, commit as `P<x.y>: <summary>`.
4. Update this file: tick the box, add a log line, set **Next task**, and note any new
   decisions or known issues. Commit again (or amend into the same commit).
5. If a task is too large, split it in PLAN.md (`P3.7a`, `P3.7b`) and record that here.

## Known issues / notes for the next session
- LLVM can't emit WasmGC, so the browser target starts on linear memory (PLAN P8.6 and P8.7).
- lld isn't installed (separate Homebrew `lld` formula); needed from P8.1.
- The unwinding strategy (LLVM EH vs error-return lowering) is decided in P3.19.

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
- [ ] P2.6 LLVM codegen v0
- [ ] P2.7 `ppl build` / `ppl run` end to end (**M1: hello world**)

### Phase 3: Core language
- [ ] P3.1 Allocator v0 and string runtime
- [ ] P3.2 char type and template literals
- [ ] P3.3 Format specs
- [ ] P3.4 Structs
- [ ] P3.5 Arrays, fixed arrays, tuples, ranges
- [ ] P3.6 Null and narrowing infrastructure
- [ ] P3.7 Enums and basic match
- [ ] P3.8 Advanced patterns and switch
- [ ] P3.9 Classes I
- [ ] P3.10 Classes II: inheritance
- [ ] P3.11 Interfaces
- [ ] P3.12 Unions and literal types
- [ ] P3.13 Generics I: functions
- [ ] P3.14 Generics II: types and where
- [ ] P3.15 Closures
- [ ] P3.16 Function features
- [ ] P3.17 Operators and equality protocols
- [ ] P3.18 Distinct types, aliases, immutability
- [ ] P3.19 Exceptions I
- [ ] P3.20 Exceptions II
- [ ] P3.21 Panics and stack traces
- [ ] P3.22 Core collections and Array API
- [ ] P3.23 Iterators and generators
- [ ] P3.24 Modules
- [ ] P3.25 Destructuring and spread
- [ ] P3.26 `using` and Disposable

### Phase 4: Memory management
- [ ] P4.1 Shadow stack and type maps
- [ ] P4.2 Mark-sweep GC
- [ ] P4.3 Generational and incremental GC
- [ ] P4.4 Weak references and finalization
- [ ] P4.5 Unsafe and pointers
- [ ] P4.6 Arenas

### Phase 5: Async and concurrency
- [ ] P5.1 Event loop
- [ ] P5.2 async/await
- [ ] P5.3 Async generators and `await using`
- [ ] P5.4 Cancellation
- [ ] P5.5 Workers
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
