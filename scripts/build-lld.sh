#!/usr/bin/env bash
# Builds lld's static libraries for the LLVM on PATH (`llvm-config`), into <prefix>, so pico
# can link them (`PICO_LLD_PREFIX=<prefix>`). For macOS, where Homebrew's `lld` formula has
# only shared libraries, built against its newest `llvm` rather than `llvm@23`.
# Needs cmake; downloads the llvm-project release matching `llvm-config --version`.
#
# Usage: scripts/build-lld.sh <prefix>
set -euo pipefail
[ $# -eq 1 ] || { echo "usage: $0 <prefix>" >&2; exit 2; }
mkdir -p "$1"
prefix=$(cd "$1" && pwd)
v=$(llvm-config --version)
llvm=$(llvm-config --prefix)
work="$prefix/.build"
mkdir -p "$work"
src="$work/llvm-project-$v.src"
if [ ! -d "$src/lld" ]; then
  tarball="$work/llvm-project-$v.src.tar.xz"
  curl -fL --retry 5 -o "$tarball" \
    "https://github.com/llvm/llvm-project/releases/download/llvmorg-$v/llvm-project-$v.src.tar.xz"
  # Only what a standalone lld build reads.
  tar -C "$work" -xJf "$tarball" "llvm-project-$v.src/lld" "llvm-project-$v.src/cmake" \
    "llvm-project-$v.src/libunwind/include" "llvm-project-$v.src/llvm/cmake" \
    "llvm-project-$v.src/llvm/utils/lit"
  rm "$tarball"
fi
extra=()
[ "$(uname -s)" = Darwin ] && extra+=(-DCMAKE_OSX_DEPLOYMENT_TARGET=13.0)
cmake -S "$src/lld" -B "$work/build" \
  -DCMAKE_BUILD_TYPE=Release \
  -DCMAKE_INSTALL_PREFIX="$prefix" \
  -DLLVM_DIR="$llvm/lib/cmake/llvm" \
  -DLLVM_INCLUDE_TESTS=OFF \
  -DBUILD_SHARED_LIBS=OFF \
  "${extra[@]}"
jobs=$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 4)
cmake --build "$work/build" -j "$jobs"
cmake --install "$work/build"
rm -rf "$work"
test -f "$prefix/lib/liblldELF.a"
echo "lld $v in $prefix"
