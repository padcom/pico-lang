# Progress

> **Starting a new session?** Paste this prompt:
>
> *"Read PROGRESS.md, then do the **Next task** from PLAN.md. Read only the spec sections that
> task lists. When the tests pass, commit, update PROGRESS.md and stop."*

## Next task
**P1.1: Toolchain setup** (see [PLAN.md](PLAN.md#p11-toolchain-setup))

Before starting it:
- Rust and LLVM aren't installed yet (only clang, node and deno are). Ask before installing.

## Current state
| Area | State |
|------|-------|
| Spec | ✅ complete: 16 chapters, 162 decisions, no open questions |
| Compiler | not started |
| Last green `scripts/test.sh` | n/a |
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
- The unwinding strategy (LLVM EH vs error-return lowering) is decided in P3.19.

## Checklist

### Phase 1: Foundation
- [ ] P1.1 Toolchain setup
- [ ] P1.2 Workspace skeleton
- [ ] P1.3 Diagnostics
- [ ] P1.4 Lexer: core tokens
- [ ] P1.5 Lexer: templates, multiline strings, regex
- [ ] P1.6 Statement-termination rule
- [ ] P1.7 AST and pretty-printer
- [ ] P1.8 Parser: expressions
- [ ] P1.9 Parser: statements and blocks
- [ ] P1.10 Parser: declarations and types
- [ ] P1.11 Parser: patterns, match, switch, try
- [ ] P1.12 Parser error recovery
- [ ] P1.13 Golden-test harness

### Phase 2: First executable
- [ ] P2.1 Name resolution
- [ ] P2.2 Type checker: primitives and functions
- [ ] P2.3 MIR design and lowering
- [ ] P2.4 MIR interpreter
- [ ] P2.5 C runtime v0
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
| 2026-10-31 | Plan | PLAN.md split into ~95 session-sized tasks, and PROGRESS.md created. |
