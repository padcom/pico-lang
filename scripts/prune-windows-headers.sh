#!/usr/bin/env bash
# Rebuilds third_party/zig-libc/include/any-windows-any as the subset of Zig's Windows headers
# that ppl compiles against (decision 248): the runtime, Mbed TLS, MinGW's CRT and winpthreads,
# and compiler-rt, for windows-x64 and windows-arm64.
#
#   scripts/prune-windows-headers.sh <zig-source-dir>
#
# Run it after changing which headers any of these include (a missing header is a compile
# error when building for Windows).
set -euo pipefail
cd "$(dirname "$0")/.."
source scripts/env.sh
zig="${1:?usage: $0 <zig source dir, e.g. an unpacked zig-0.15.1 tarball>}"
full="$zig/lib/libc/include/any-windows-any"
dest=third_party/zig-libc/include/any-windows-any
[ -f "$full/windows.h" ] || { echo "no $full/windows.h" >&2; exit 1; }

rm -rf "$dest" && cp -r "$full" "$dest"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
export TMPDIR="$work" PPL_DEPFILES=1
printf 'println("hello")\n' > "$work/hello.ppl"
cargo build -q -p ppl-cli
for t in windows-x64 windows-arm64; do
  ./target/debug/ppl build --target "$t" "$work/hello.ppl" -o "$work/hello-$t.exe"
done

# Every header a dependency file names, relative to the header directory.
abs=$(cd "$dest" && pwd)
find "$work/ppl-cache" -name '*.d' -print0 | xargs -0 cat | tr ' \\' '\n\n' \
  | grep -F "any-windows-any/" | sed "s|.*any-windows-any/||" | sort -u > "$work/keep"
[ -s "$work/keep" ] || { echo "no dependencies recorded" >&2; exit 1; }
(cd "$abs" && find . -type f | sed 's|^\./||' | sort) > "$work/all"
comm -23 "$work/all" "$work/keep" | (cd "$abs" && xargs -r rm -f)
find "$dest" -type d -empty -delete
echo "kept $(wc -l < "$work/keep") of $(wc -l < "$work/all") headers ($(du -sh "$dest" | cut -f1))"
