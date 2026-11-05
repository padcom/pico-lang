#!/usr/bin/env bash
# Builds a release archive for this host (P10.10, decisions 296-297):
#   dist/pico-<version>-<host>.tar.gz  containing
#   pico-<version>-<host>/bin/pico     (with the runtime, vendored C sources and stdlib inside)
#   pico-<version>-<host>/LICENSE, README.md, licenses/ (the vendored sources' licenses)
# The compiler unpacks its resources into $TMPDIR/pico-cache/home on first use, and compiles C
# with the clang linked into it (decision 298): it needs no LLVM installation, only the system
# libraries (and, for host builds on macOS, the Xcode command line tools' `cc`).
#
# Usage: scripts/package.sh [--version V] [--check]
#   --check  extract the archive into a temporary directory and build and run a program with it
set -euo pipefail
cd "$(dirname "$0")/.."
source scripts/env.sh

version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
check=0
while [ $# -gt 0 ]; do
  case "$1" in
    --version) version="$2"; shift 2 ;;
    --check) check=1; shift ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done
version=${version#v}

case "$(uname -s)" in
  Darwin) os=macos ;;
  Linux) os=linux ;;
  # (Windows: from an MSYS2 shell, clang64 or clangarm64, with MSYS2's LLVM 23.)
  MINGW*|MSYS*) os=windows ;;
  *) echo "unsupported host: $(uname -s)" >&2; exit 1 ;;
esac
case "$(uname -m)" in
  arm64|aarch64) arch=arm64 ;;
  x86_64|amd64) arch=x64 ;;
  *) echo "unsupported architecture: $(uname -m)" >&2; exit 1 ;;
esac
name="pico-$version-$os-$arch"
exe=pico
[ "$os" = windows ] && exe=pico.exe

# Only system libraries at run time: LLVM's other dependencies linked statically (a search
# directory holding only their static libraries, ahead of the others), and the libraries nothing
# uses (LLVM's z3) dropped. On Linux, libffi (LLVM's interpreter) and libxml2 (lld's Windows
# manifests, with what it uses: ICU, liblzma) too; macOS has its own.
static="$PWD/target/package/static"
rm -rf "$static" && mkdir -p "$static"
if [ "$os" = macos ]; then
  wanted=(zstd)
  extra=(-C link-arg=-Wl,-dead_strip_dylibs)
elif [ "$os" = windows ]; then
  # Everything but the system's DLLs statically: MSYS2's libc++, libunwind, winpthreads, and
  # LLVM's zstd, libxml2 (with libiconv, liblzma), libffi and zlib. (`-static` doesn't help:
  # rustc's `-Bdynamic` comes first. lld takes the first search directory with any form of a
  # library, so their static archives in `$static`, searched first, win.) `llvm-config` names
  # zstd's import library (`-lzstd.dll`), so a static copy takes that name too.
  wanted=(zstd xml2 iconv lzma ffi z c++ unwind pthread winpthread)
  extra=()
  for l in iconv lzma z winpthread; do extra+=(-C "link-arg=-l$l"); done
else
  wanted=(zstd ffi xml2 icuuc icudata lzma)
  extra=(-C link-arg=-Wl,--as-needed)
  # (libxml2's own dependencies, after it on the link line.)
  for l in icuuc icudata lzma; do extra+=(-C "link-arg=-l$l"); done
fi
for l in "${wanted[@]}"; do
  for d in "$(brew --prefix 2>/dev/null)/lib" /usr/lib/"$(uname -m)"-linux-gnu /usr/lib /usr/local/lib \
    "${MINGW_PREFIX:-/nonexistent}/lib"; do
    if [ -f "$d/lib$l.a" ]; then cp "$d/lib$l.a" "$static/"; break; fi
  done
  [ -f "$static/lib$l.a" ] || { echo "lib$l.a not found (install its static library)" >&2; exit 1; }
done
[ "$os" = windows ] && cp "$static/libzstd.a" "$static/libzstd.dll.a"
# (A target directory of its own: other RUSTFLAGS than development builds. On Windows, rustc
# takes Windows paths, not MSYS2's.)
static_flag=$static
[ "$os" = windows ] && static_flag=$(cygpath -m "$static")
RUSTFLAGS="-L native=$static_flag ${extra[*]}" \
  cargo build --release -p pico-cli --features embed --target-dir target/package
bin=target/package/release/$exe

# The libraries it loads must all be the system's.
if [ "$os" = macos ]; then
  libs=$(otool -L "$bin" | tail -n +2 | awk '{print $1}')
  bad=$(echo "$libs" | grep -v -E '^(/usr/lib/|/System/)' || true)
elif [ "$os" = windows ]; then
  libs=$(objdump -p "$bin" | sed -n 's/.*DLL Name: //p')
  bad=$(echo "$libs" | grep -v -i -E '^(kernel32|kernelbase|ntdll|user32|advapi32|shell32|ole32|oleaut32|ws2_32|crypt32|bcrypt|bcryptprimitives|psapi|version|userenv|dbghelp|secur32|shlwapi|uuid|ucrtbase|msvcrt|api-ms-win-[a-z0-9-]+)\.dll$' || true)
else
  libs=$(readelf -d "$bin" | sed -n 's/.*Shared library: \[\(.*\)\]/\1/p')
  bad=$(echo "$libs" | grep -v -E '^(libc|libm|libdl|librt|libpthread|libgcc_s|libstdc\+\+|libz|ld-linux.*)\.so' || true)
fi
[ -z "$bad" ] || { echo "pico needs non-system libraries:" >&2; echo "$bad" >&2; exit 1; }

out="dist/$name"
rm -rf "$out" "dist/$name.tar.gz" "dist/$name.zip"
mkdir -p "$out/bin"
cp "$bin" "$out/bin/"
cp LICENSE "$out/"
# The vendored sources' license files, as licenses/<path in third_party>.
(cd third_party && find . -maxdepth 3 -type f \( -iname 'licen[cs]e*' -o -iname 'copying*' \
  -o -iname 'copyright*' -o -iname 'notice*' \)) | sed 's|^\./||' | while read -r f; do
  mkdir -p "$out/licenses/$(dirname "$f")"
  cp "third_party/$f" "$out/licenses/$f"
done
# LLVM, lld and clang are linked into pico.
mkdir -p "$out/licenses/llvm"
cp crates/pico-clang/vendor/LICENSE.TXT "$out/licenses/llvm/LICENSE.TXT"
cat > "$out/README.md" <<EOF
# pico $version ($os-$arch)

The Pico Programming Language compiler. Put \`bin/pico\` on your PATH. On first use it unpacks
its runtime, C library sources, clang's headers and the standard library into
\`\$TMPDIR/pico-cache/home\`. It compiles C (the runtime, C libraries, \`c:\` imports) with the
clang inside it: no LLVM installation is needed.

pico is MIT-licensed (\`LICENSE\`). The vendored sources inside it keep their own licenses,
in \`licenses/\`.

EOF
if [ "$os" = macos ]; then
  echo 'Programs for macOS itself need the Xcode command line tools (`xcode-select --install`).' >> "$out/README.md"
elif [ "$os" = windows ]; then
  echo 'Windows 10 or later. The cache is in `%TEMP%\pico-cache`.' >> "$out/README.md"
else
  echo 'Linux: glibc 2.39 or later.' >> "$out/README.md"
fi
if [ "$os" = windows ]; then
  # (Windows' own bsdtar writes zip files.)
  (cd dist && "$(cygpath -u "$SYSTEMROOT")/System32/tar.exe" -a -cf "$name.zip" "$name")
  archive="dist/$name.zip"
else
  tar -C dist -czf "dist/$name.tar.gz" "$name"
  archive="dist/$name.tar.gz"
fi
echo "$archive"

if [ "$check" = 1 ]; then
  tmp=$(mktemp -d)
  trap 'rm -rf "$tmp"' EXIT
  if [ "$os" = windows ]; then
    zip_path=$(cygpath -w "$PWD/$archive")
    (cd "$tmp" && "$(cygpath -u "$SYSTEMROOT")/System32/tar.exe" -xf "$zip_path")
  else
    tar -C "$tmp" -xzf "$archive"
  fi
  # (A cache of its own: the check unpacks the bundled resources; Windows asks TMP/TEMP.)
  export TMPDIR="$tmp/cache"
  mkdir -p "$TMPDIR"
  if [ "$os" = windows ]; then
    TMP=$(cygpath -w "$TMPDIR")
    export TMP TEMP="$TMP"
  fi
  printf 'println("hello from pico")\n' > "$tmp/hello.pi"
  # (No installed clang: the one inside pico.)
  unset PICO_CLANG
  pico="$tmp/$name/bin/$exe"
  got=$(cd "$tmp" && "$pico" run hello.pi)
  [ "$got" = "hello from pico" ] || { echo "check failed: got '$got'" >&2; exit 1; }
  # (lld is optional when building on macOS: without it, pico can't link other targets.)
  if [ "$os" != macos ] || [ -n "${PICO_LLD_PREFIX:-}" ]; then
    (cd "$tmp" && "$pico" build --target wasm32-wasi hello.pi -o hello.wasm)
    [ -s "$tmp/hello.wasm" ] || { echo "check failed: no hello.wasm" >&2; exit 1; }
  else
    echo "check: skipped wasm32-wasi (built without lld; set PICO_LLD_PREFIX)"
  fi
  ls -d "$TMPDIR"/pico-cache/home/*/stdlib >/dev/null || { echo "check failed: not unpacked" >&2; exit 1; }
  echo "check: ok"
fi
