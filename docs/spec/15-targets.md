# 15. Targets

## 15.1 Target list
Targets are named by **LLVM triples**, with short aliases for convenience:

| Triple | Alias | Notes |
|--------|-------|-------|
| `x86_64-unknown-linux-gnu` | `linux-x64` | dynamic glibc |
| `aarch64-unknown-linux-gnu` | `linux-arm64` | dynamic glibc |
| `x86_64-apple-darwin` | `macos-x64` | |
| `aarch64-apple-darwin` | `macos-arm64` | |
| `x86_64-w64-windows-gnu` | `windows-x64` | MinGW ABI |
| `aarch64-w64-windows-gnu` | `windows-arm64` | MinGW ABI |
| `wasm32-unknown-unknown` | `wasm32-browser` | WasmGC + JS glue + `.d.ts` |
| `wasm32-wasip2` | `wasm32-wasi` | WASI preview 2, ppl GC |
| `riscv64…` / bare metal | later | `--no-gc` subset |

```
ppl build --target x86_64-w64-windows-gnu
ppl build --target windows-x64          # same thing
```

## 15.2 Linking
- **Dynamic system libc** on every platform: glibc on Linux, libSystem on macOS, msvcrt/UCRT
  through MinGW on Windows.
- **Cross-compiling from any host** with no SDK installed: `ppl` bundles lld plus the linking
  stubs it needs (glibc version stubs, macOS `.tbd` files, MinGW import libraries), the same way
  Zig does.
- Windows uses the **MinGW (GNU) ABI**. C libraries built for MSVC may not link and need MinGW
  builds.

## 15.3 Minimum OS versions (modern only)
| OS | Minimum |
|----|---------|
| macOS | 13 (Ventura) |
| Windows | 10 |
| Linux | kernel 5.x, glibc 2.31 |

Newer facilities can be used where they exist (io_uring on Linux, for example).

## 15.4 Platform conditionals
At comptime: `target.os` (`"linux" | "macos" | "windows" | "wasi" | "browser"`), `target.arch`,
`target.triple`. Modules can also be platform-specific by file name: `fs.linux.ppl`,
`fs.windows.ppl`, `fs.wasm.ppl`.

## Open questions
- Is the glibc version a build parameter (`--glibc 2.31`)?
- Is a musl static option (`linux-x64-musl`) wanted for single-file deploys?
- Android / iOS?
