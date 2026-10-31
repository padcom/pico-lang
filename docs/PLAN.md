# Roadmap

## Phase 0: Specification (current)
- Record every design decision in [DECISIONS.md](DECISIONS.md).
- Write the language spec chapter by chapter in [spec/](spec/). Each chapter lists its own
  **Open questions**, and the interview continues until none are left.
- Exit criterion: the spec is approved and covers lexing, types, semantics, runtime and tooling
  well enough to start implementation.

## Phase 1: Bootstrap compiler (Rust + LLVM)
- Cargo workspace: `ppl-lexer`, `ppl-parser`, `ppl-types`, `ppl-codegen` (inkwell/LLVM),
  `ppl-runtime` (GC, strings, panics), `ppl-cli`.
- MVP subset: ints/floats/bool/string, functions, `if`/`while`/`for-of` over ranges, blocks as
  expressions, `println` → native binary on the macOS host.
- Golden tests: `tests/*.ppl` + expected stdout, plus `cargo test`.

## Phase 2: Core language
- Structs, classes, interfaces, enums + `match`, unions/narrowing, generics (monomorphization),
  checked `throws`, arrays/Map/Set/tuples, template literals, modules.
- Tracing GC with per-thread heaps.

## Phase 3: Targets
- Cross-compilation with bundled lld: linux/macos/windows × x86_64/arm64.
- wasm32-wasi, then wasm32 browser with JS glue and generated `.d.ts`; WasmGC.

## Phase 4: Advanced features
- async/await (stackless state machines) + event loop; isolated workers + message passing.
- comptime execution, decorators as comptime functions, reflection, `JSON.parse<T>`.
- C FFI, automatic import of C headers, reading `.d.ts` files.

## Phase 5: Tooling & ecosystem
- `ppl test`, `ppl fmt`, LSP + VS Code extension, REPL/JIT `ppl run`.
- Package manager (`ppl add`, lockfile), batteries-included stdlib.
- Later: `--no-gc` embedded subset, riscv64.
