#!/usr/bin/env bash
# Builds a release archive for this host (P10.10, decisions 296-297):
#   dist/pico-<version>-<host>.tar.gz  containing
#   pico-<version>-<host>/bin/pico     (with the runtime, vendored C sources and stdlib inside)
#   pico-<version>-<host>/LICENSE, README.md, licenses/ (the vendored sources' licenses)
# The compiler unpacks its resources into $TMPDIR/pico-cache/home on first use. It still needs
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

cargo build --release -p pico-cli --features embed

out="dist/$name"
rm -rf "$out" "dist/$name.tar.gz"
mkdir -p "$out/bin"
cp target/release/pico "$out/bin/"
cp LICENSE "$out/"
# The vendored sources' license files, as licenses/<path in third_party>.
(cd third_party && find . -maxdepth 3 -type f \( -iname 'licen[cs]e*' -o -iname 'copying*' \
  -o -iname 'copyright*' -o -iname 'notice*' \)) | sed 's|^\./||' | while read -r f; do
  mkdir -p "$out/licenses/$(dirname "$f")"
  cp "third_party/$f" "$out/licenses/$f"
done
cat > "$out/README.md" <<EOF
# pico $version ($os-$arch)

The Pico Programming Language compiler. Put \`bin/pico\` on your PATH. On first use it unpacks
its runtime, C library sources and standard library into \`\$TMPDIR/pico-cache/home\`.

pico is MIT-licensed (\`LICENSE\`). The vendored sources inside it keep their own licenses,
in \`licenses/\`.

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
  # (A cache of its own: the check unpacks the bundled resources.)
  export TMPDIR="$tmp/cache"
  mkdir -p "$TMPDIR"
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
  ls -d "$TMPDIR"/pico-cache/home/*/stdlib >/dev/null || { echo "check failed: not unpacked" >&2; exit 1; }
  echo "check: ok"
fi
