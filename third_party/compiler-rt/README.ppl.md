# compiler-rt builtins (vendored for ppl)

From LLVM 23.1.4 (`llvm-project-23.1.4.src.tar.xz` from the GitHub release `llvmorg-23.1.4`,
attestation verified with `gh attestation verify`): `compiler-rt/LICENSE.TXT` and the top-level
`*.c`, `*.h`, `*.inc` files of `compiler-rt/lib/builtins` (no architecture subdirectories),
unmodified.

ppl compiles the generic, 128-bit-float and x86 80-bit lists from `lib/builtins/CMakeLists.txt`
per target (`crates/ppl-driver/src/builtins.rs`, decision 247) and links them, as needed, into
every program it links with lld: what libgcc does for gcc.

License: Apache-2.0 WITH LLVM-exception (`LICENSE.TXT`).
