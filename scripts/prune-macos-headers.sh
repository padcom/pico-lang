#!/usr/bin/env bash
# Rebuilds third_party/zig-libc/include/any-macos-any as the subset of Zig's macOS headers
# that ppl compiles against (the runtime and Mbed TLS), for macos-x64 and macos-arm64.
#
#   scripts/prune-macos-headers.sh <zig-source-dir>
#
# Run it after changing which headers the runtime includes (a missing header is a compile
# error when building for macOS).
set -euo pipefail
cd "$(dirname "$0")/.."
source scripts/env.sh
zig="${1:?usage: $0 <zig source dir, e.g. an unpacked zig-0.15.1 tarball>}"
full="$zig/lib/libc/include/any-macos-any"
dest=third_party/zig-libc/include/any-macos-any
[ -f "$full/stdio.h" ] || { echo "no $full/stdio.h" >&2; exit 1; }

rm -rf "$dest" && cp -r "$full" "$dest"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
export TMPDIR="$work" PPL_DEPFILES=1
printf 'println("hello")\n' > "$work/hello.ppl"
cargo build -q -p ppl-cli
for t in macos-x64 macos-arm64; do
  ./target/debug/ppl build --target "$t" "$work/hello.ppl" -o "$work/hello-$t"
done

# Every header a dependency file names, relative to the header directory.
abs=$(cd "$dest" && pwd)
find "$work/ppl-cache" -name '*.d' -print0 | xargs -0 cat | tr ' \\' '\n\n' \
  | grep -F "any-macos-any/" | sed "s|.*any-macos-any/||" | sort -u > "$work/keep"
[ -s "$work/keep" ] || { echo "no dependencies recorded" >&2; exit 1; }
(cd "$abs" && find . -type f | sed 's|^\./||' | sort) > "$work/all"
comm -23 "$work/all" "$work/keep" | (cd "$abs" && xargs -r rm -f)
find "$dest" -type d -empty -delete
echo "kept $(wc -l < "$work/keep") of $(wc -l < "$work/all") headers ($(du -sh "$dest" | cut -f1))"
