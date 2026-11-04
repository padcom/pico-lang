# 15. Targets

## 15.1 Target list
Targets are named by **LLVM triples**, with short aliases for convenience:

| Triple | Alias | Notes |
|--------|-------|-------|
| `x86_64-unknown-linux-gnu` | `linux-x64` | dynamic glibc |
| `aarch64-unknown-linux-gnu` | `linux-arm64` | dynamic glibc |
| `x86_64-unknown-linux-musl` | `linux-x64-musl` | fully static, single file |
| `aarch64-unknown-linux-musl` | `linux-arm64-musl` | fully static, single file |
| `x86_64-apple-darwin` | `macos-x64` | |
| `aarch64-apple-darwin` | `macos-arm64` | |
| `x86_64-w64-windows-gnu` | `windows-x64` | MinGW ABI |
| `aarch64-w64-windows-gnu` | `windows-arm64` | MinGW ABI |
| `wasm32-unknown-unknown` | `wasm32-browser` | WasmGC + JS glue + `.d.ts` |
| `wasm32-wasip2` | `wasm32-wasi` | WASI preview 2, pico GC |
| `aarch64-linux-android` | `android-arm64` | after v1 |
| `aarch64-apple-ios` | `ios-arm64` | after v1 |
| `riscv64…` / bare metal | later | `--no-gc` subset |

```
pico build --target x86_64-w64-windows-gnu
pico build --target windows-x64          # same thing
```

## 15.2 Linking
- **Dynamic system libc** on every platform: glibc on Linux, libSystem on macOS, msvcrt/UCRT
  through MinGW on Windows.
- **Cross-compiling from any host** with no SDK installed: `pico` bundles lld plus the linking
  stubs it needs (glibc version stubs, macOS `.tbd` files, MinGW import libraries), the same way
  Zig does.
- Windows uses the **MinGW (GNU) ABI**. C libraries built for MSVC may not link and need MinGW
  builds.

The glibc version is selected with a triple suffix, as in Zig: `x86_64-unknown-linux-gnu.2.35`.
Without one, binaries link against glibc **2.31** stubs.

## 15.3 Minimum OS versions (modern only)
| OS | Minimum |
|----|---------|
| macOS | 13 (Ventura) |
| Windows | 10 |
| Linux | kernel 5.x, glibc 2.31 |

Newer facilities can be used where they exist (io_uring on Linux, for example).

## 15.4 Platform conditionals
At comptime: `target.os` (`"linux" | "macos" | "windows" | "wasi" | "browser"`), `target.arch`,
`target.triple`. Modules can also be platform-specific by file name: `fs.linux.pi`,
`fs.windows.pi`, `fs.wasm.pi`.

`import … from "./fs"` picks `fs.<os>.pi` (`target.os`) first, then `fs.wasm.pi` on
`wasm32-wasi` and `wasm32-browser`, then `fs.pi`, then `fs/index.pi` (decision 260).
