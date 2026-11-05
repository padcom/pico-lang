# Development setup

## Pinned versions

| Tool | Version | Source |
|------|---------|--------|
| Rust (rustc, cargo, rustfmt, clippy) | 1.99.0 | rustup, pinned by `rust-toolchain.toml` |
| rustup | 1.29.1 | Homebrew `rustup` (keg-only) |
| LLVM (llvm-config, clang, libLLVM) | 23.1.x | Homebrew `llvm@23` (keg-only) on macOS, apt.llvm.org `llvm-23` on Linux |
| inkwell | 0.11 with feature `llvm23-1` | crates.io (added in P2.6) |
| llvm-sys | 231.x (pulled in by inkwell) | crates.io |

Development host: Linux arm64 (`aarch64-unknown-linux-gnu`, a VM on macOS) from P8.0 on; macOS
arm64 (`aarch64-apple-darwin`) before that. CI runs `scripts/test.sh` on both (see below).

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

## Setup (Linux, Debian/Ubuntu)

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
wget -qO /tmp/llvm.sh https://apt.llvm.org/llvm.sh && sudo bash /tmp/llvm.sh 23   # clang-23, lld-23, …
sudo apt-get install -y build-essential llvm-23-dev libpolly-23-dev libzstd-dev zlib1g-dev \
  liblld-23-dev libc6-amd64-cross qemu-user-static qemu-user-binfmt file
```

`liblld-23-dev` is required: lld is linked into pico (crate `pico-lld`). Building for any
`linux-gnu` target needs no glibc at all (pico generates stubs, P8.2a); `libc6-amd64-cross` is
only there so qemu can run x86-64 programs (`QEMU_LD_PREFIX=/usr/x86_64-linux-gnu ./prog`).

`source scripts/env.sh` then sets `LLVM_SYS_231_PREFIX=/usr/lib/llvm-23` and puts
`/usr/lib/llvm-23/bin` (so `clang` is clang 23) and `~/.cargo/bin` on `PATH`.

## CI

`.github/workflows/test.yml` runs `scripts/test.sh` on GitHub's `macos-latest` (arm64) and
`ubuntu-24.04-arm` runners on every push to `main` and every pull request. With development on
Linux, the macOS job is what catches regressions in the kqueue and Mach-O paths. Check a run with
`gh run list --limit 3` and `gh run view <id> --log-failed`.

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
cargo run -p pico-cli -- run hello.pi            # native (built into $TMPDIR/pico-cache)
cargo run -p pico-cli -- run --interp hello.pi   # MIR interpreter
cargo run -p pico-cli -- build hello.pi -o hello --release
cargo run -p pico-cli -- build --emit mir hello.pi     # or --emit llvm-ir
```

On Linux, native builds (host and `--target linux-x64` / `linux-arm64`) are linked by the lld
inside pico against generated glibc stubs (default glibc 2.31, `--target linux-x64.2.35` for
another). The C runtime and Mbed TLS are compiled for the target and glibc version by clang 23
on first use, against Zig's glibc headers in `third_party/zig-libc`, and cached in
`$TMPDIR/pico-cache/runtime/<triple>-<hash>` (about 2 s); the stubs are cached in
`$TMPDIR/pico-cache/glibc/`. `--target linux-x64-musl` / `linux-arm64-musl` link fully
statically with musl, compiled from `third_party/musl` on first use (about 5 s, cached in
`$TMPDIR/pico-cache/musl/`). Compiler builtins (compiler-rt, `third_party/compiler-rt`) are
compiled per target the same way and linked into every program, like libgcc. On macOS, builds still link
with the system `cc` until P8.4; lld is optional there (`PICO_LLD_PREFIX` names an lld
installation with `lib/liblldELF.a`). The interpreter's copy of the runtime is compiled by
`pico-runtime`'s build script (with `-mmacosx-version-min=13.0` on macOS).

```sh
cargo run -p pico-cli -- build --target linux-x64 hello.pi -o hello-x64
QEMU_LD_PREFIX=/usr/x86_64-linux-gnu ./hello-x64
PICO_TEST_TARGET=linux-x64 cargo test -p pico-tests --test golden   # all golden tests, ~4 min
PICO_TEST_TARGET=linux-arm64-musl cargo test -p pico-tests --test golden
```

A fixture qemu-user can't run faithfully gets `// not-under-qemu: why` (it's then pending in
such runs).

## Releases
`scripts/package.sh` builds `dist/pico-<version>-<host>.tar.gz` for this host (`--check` also
extracts it and builds a program with it): `bin/pico`, built with `--features embed` so that it
carries `runtime/`, `third_party/`, `stdlib/` and the host runtime libraries and unpacks them into
`$TMPDIR/pico-cache/home/` on first use (decision 297), plus `LICENSE` and the vendored sources'
licenses. `PICO_HOME` (or `share/pico` next to `bin/`) overrides the bundle. The `pico` executable
compiles C with the clang linked into it (`pico clang …`, crate `pico-clang`, decision 298) when
LLVM has clang's static libraries (Homebrew's `llvm@23` has them; apt: `libclang-23-dev`);
`PICO_CLANG` names an installed clang instead. Test binaries run the driver in-process and use the
LLVM installation's clang. Release archives need no LLVM installation and only system libraries
(the script checks with `otool -L`/`readelf -d`; release builds go to `target/package`).

To publish: set `version` in `Cargo.toml` and `editors/vscode/package.json`, and the download
commands in `website/index.html` (section "Get started"), commit, then `git tag v<version> &&
git push --tags`. The release also gets the VS Code extension (`pico-lang-<version>.vsix`, ID `padcom.pico-lang`, built by
`scripts/vscode-package.sh` in the publish job, which checks that its version matches the tag).
`.github/workflows/release.yml` builds the macOS arm64, Linux x64/arm64 and Windows x64/arm64
archives (Windows in MSYS2's clang64/clangarm64 with Rust's gnullvm toolchain, decision 310), tries each
on a runner with only LLVM installed, and creates the GitHub release (a tag with a `-`, like
`v0.2.0-rc.1`, makes a prerelease). Run it by hand (Actions → release → Run workflow) to build and
try the archives without publishing.

## Not installed yet

These come with the tasks that need them (ask before installing):

- **lld on macOS**: Homebrew's `llvm@23` doesn't ship it, and the separate `lld` formula has
  only shared libraries (for its newest `llvm`). `scripts/build-lld.sh <prefix>` builds the static
  ones for llvm@23 (about 10 minutes); then `export PICO_LLD_PREFIX=<prefix>`. The release workflow
  does this.
- **wasmtime** 50.0.1 (P8.5a), for running `wasm32-wasi` programs: the official release binary in
  `~/.local/bin` (no root; its GitHub build attestation checked with
  `gh attestation verify <tarball> --repo bytecodealliance/wasmtime`). CI installs the same
  version. Programs need Wasm exception handling: `wasmtime run -W exceptions=y prog.wasm`
  (`PICO_TEST_TARGET=wasm32-wasi cargo test -p pico-tests --test golden` runs them that way).
- **Wine**: optional, for running Windows test binaries (P8.3).
