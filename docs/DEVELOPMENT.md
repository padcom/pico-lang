# Development setup

## Pinned versions

| Tool | Version | Source |
|------|---------|--------|
| Rust (rustc, cargo, rustfmt, clippy) | 1.99.0 | rustup, pinned by `rust-toolchain.toml` |
| rustup | 1.29.1 | Homebrew `rustup` (keg-only) |
| LLVM (llvm-config, clang, libLLVM) | 23.1.3 | Homebrew `llvm@23` (keg-only) |
| inkwell | 0.11 with feature `llvm23-1` | crates.io (added in P2.6) |
| llvm-sys | 231.x (pulled in by inkwell) | crates.io |

Host used for development: macOS arm64 (`aarch64-apple-darwin`).

The plan originally suggested LLVM 18. inkwell 0.11 supports LLVM 16 to 23, so we use the current
Homebrew LLVM (23) and avoid keeping an old one around. If you change the LLVM major version, change
the inkwell feature (`llvmNN-1`) and the `LLVM_SYS_NNN_PREFIX` variable to match.

## Setup (macOS)

```sh
brew install rustup llvm@23
rustup toolchain install stable   # rust-toolchain.toml fetches 1.99.0 on first use anyway
```

Homebrew installs both formulas keg-only, so they aren't on `PATH`. Source the env script in every
shell you build in (or add the equivalent lines to your shell profile):

```sh
source scripts/env.sh
```

It sets:

| Variable | Value | Why |
|----------|-------|-----|
| `PATH` | prepends `$(brew --prefix)/opt/rustup/bin` and `$(brew --prefix)/opt/llvm@23/bin` | `cargo`, `rustc`, `llvm-config`, and Homebrew's `clang` 23 |
| `LLVM_SYS_231_PREFIX` | `$(brew --prefix)/opt/llvm@23` | tells llvm-sys (via inkwell) where LLVM lives |

Variables you've already set are kept, so on other systems export `LLVM_SYS_231_PREFIX` (and put
`cargo` on `PATH`) yourself before sourcing it.

With the env script sourced, `clang` is Homebrew clang 23 (same version as the LLVM we link), not
Apple clang. The C runtime should be built with it.

## Check

```sh
source scripts/env.sh
cargo --version        # cargo 1.99.0
rustc --version        # rustc 1.99.0
llvm-config --version  # 23.1.3
clang --version        # Homebrew clang version 23.1.3
```

A throwaway crate depending on `inkwell = { version = "0.11", features = ["llvm23-1"] }` builds,
links against libLLVM and prints module IR (checked during P1.1).

## Building and running programs

```sh
cargo run -p ppl-cli -- run hello.ppl            # native (built into $TMPDIR/ppl-cache)
cargo run -p ppl-cli -- run --interp hello.ppl   # MIR interpreter
cargo run -p ppl-cli -- build hello.ppl -o hello --release
cargo run -p ppl-cli -- build --emit mir hello.ppl     # or --emit llvm-ir
```

Native builds link with the system `cc` (Apple clang or Homebrew clang both work) until the
bundled lld arrives in P8.1. The C runtime in `runtime/` is compiled by `ppl-runtime`'s build
script with `-mmacosx-version-min=13.0`.

## Not installed yet

These come with the tasks that need them (ask before installing):

- **lld**: Homebrew's `llvm@23` doesn't ship it; it's the separate `lld` formula. Needed for P8.1.
- **wasmtime**: P8.5.
- **Docker / Wine**: optional, for running Linux/Windows test binaries (P8.1–P8.3).
