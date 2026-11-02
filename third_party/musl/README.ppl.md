# musl (vendored for ppl)

musl 1.2.5 (`https://musl.libc.org/releases/musl-1.2.5.tar.gz`, sha256
`a9a118bbe84d8764da0ea0d28b3ab3fae8477fc7e4085d90102b8596fc7c75e4`, signature checked against
musl's key `8364 8929 0BB6 B70F 99FF DA05 56BC DB59 3020 450F`). Files are unmodified; only a
subset is kept: `COPYRIGHT`, `README`, `VERSION`, `Makefile` (for reference), `include/`, `src/`,
`crt/`, `tools/mkalltypes.sed`, and `arch/` for `generic`, `x86_64` and `aarch64` only (other
architectures' directories are also removed from `src/` and `crt/`).

ppl compiles it itself (`crates/ppl-driver/src/musl.rs`, decision 247), following the Makefile:
the generated headers (`bits/alltypes.h`, `bits/syscall.h`, `version.h`) and the source
selection (arch-specific files replace generic ones of the same name) are reimplemented there.

License: MIT (`COPYRIGHT`).

To update: replace these paths from a newer release, prune the same way, and run
`PPL_TEST_TARGET=linux-arm64-musl cargo test -p ppl-tests --test golden`.
