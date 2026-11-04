#!/usr/bin/env bash
# Builds a release archive for this host (P10.10a, decision 296):
#   dist/pico-<version>-<host>.tar.gz  containing
#   pico-<version>-<host>/bin/pico
#   pico-<version>-<host>/share/pico/{runtime,third_party,stdlib,lib}
# The compiler finds its resources in share/pico next to bin/ (or in PICO_HOME). It still needs
# LLVM 23's clang at run time (and, for host builds on macOS, the system `cc`).
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
  *) echo "unsupported host: $(uname -s)" >&2; exit 1 ;;
esac
case "$(uname -m)" in
  arm64|aarch64) arch=arm64 ;;
  x86_64|amd64) arch=x64 ;;
  *) echo "unsupported architecture: $(uname -m)" >&2; exit 1 ;;
esac
name="pico-$version-$os-$arch"

cargo build --release -p pico-cli

# The host runtime libraries come from pico-runtime's build script (the newest build of it).
lib=$(ls -td target/release/build/pico-runtime-*/out 2>/dev/null | while read -r d; do
  [ -f "$d/libpico_rt.a" ] && { echo "$d"; break; }
done)
[ -n "$lib" ] || { echo "libpico_rt.a not found under target/release/build" >&2; exit 1; }

out="dist/$name"
rm -rf "$out" "dist/$name.tar.gz"
mkdir -p "$out/bin" "$out/share/pico/lib"
cp target/release/pico "$out/bin/"
cp LICENSE "$out/"
cp "$lib"/libpico_rt.a "$lib"/libpico_crypto.a "$lib"/libpico_mbedtls.a "$out/share/pico/lib/"
# (Copied as they are in the repository: the compiler finds third_party/ from runtime/include.)
cp -R runtime third_party stdlib "$out/share/pico/"
cat > "$out/README.md" <<EOF
# pico $version ($os-$arch)

The Pico Programming Language compiler. Put \`bin/\` on your PATH (keep \`share/\` next to it,
or set \`PICO_HOME\` to \`share/pico\`).

pico is MIT-licensed (\`LICENSE\`). The vendored sources in \`share/pico/third_party\` keep
their own licenses (the license files in each directory).

Requirements:
- LLVM 23's clang: Homebrew \`brew install llvm@23\` on macOS, apt.llvm.org on Linux
  (\`sudo bash llvm.sh 23\`). Set \`PICO_CLANG\` if it's somewhere else.
EOF
if [ "$os" = macos ]; then
  echo '- The Xcode command line tools (`xcode-select --install`), for linking macOS programs.' >> "$out/README.md"
fi
tar -C dist -czf "dist/$name.tar.gz" "$name"
echo "dist/$name.tar.gz"

if [ "$check" = 1 ]; then
  tmp=$(mktemp -d)
  trap 'rm -rf "$tmp"' EXIT
  tar -C "$tmp" -xzf "dist/$name.tar.gz"
  printf 'println("hello from pico")\n' > "$tmp/hello.pi"
  pico="$tmp/$name/bin/pico"
  got=$(cd "$tmp" && "$pico" run hello.pi)
  [ "$got" = "hello from pico" ] || { echo "check failed: got '$got'" >&2; exit 1; }
  # (lld is optional when building on macOS: without it, pico can't link other targets.)
  if [ "$os" = linux ] || [ -n "${PICO_LLD_PREFIX:-}" ]; then
    (cd "$tmp" && "$pico" build --target wasm32-wasi hello.pi -o hello.wasm)
    [ -s "$tmp/hello.wasm" ] || { echo "check failed: no hello.wasm" >&2; exit 1; }
  else
    echo "check: skipped wasm32-wasi (built without lld; set PICO_LLD_PREFIX)"
  fi
  echo "check: ok"
fi
