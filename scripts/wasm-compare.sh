#!/usr/bin/env bash
# Builds programs for wasm32-browser with both backends (LLVM on linear memory with the pico GC,
# and WasmGC: P8.7) and compares their module sizes and run times under Node.js (best of 3).
# Usage: scripts/wasm-compare.sh [file.pi…] (default: hello and the bench/wasm_* programs).
set -euo pipefail
cd "$(dirname "$0")/.."
source scripts/env.sh
cargo build --quiet
out=$(mktemp -d)
files=("$@")
[ ${#files[@]} -gt 0 ] || files=(tests/run/hello.pi bench/wasm_compute.pi bench/wasm_alloc.pi)
best() {
  local b=999999
  for _ in 1 2 3; do
    local s e
    s=$(date +%s%N)
    node "$1" > /dev/null
    e=$(date +%s%N)
    local ms=$(( (e - s) / 1000000 ))
    [ "$ms" -lt "$b" ] && b=$ms
  done
  echo "$b"
}
printf '%-28s %12s %12s %10s %10s\n' program "linear KiB" "WasmGC KiB" "linear ms" "WasmGC ms"
for f in "${files[@]}"; do
  name=$(basename "$f" .pi)
  ./target/debug/pico build --release --target wasm32-browser "$f" -o "$out/$name-lin" > /dev/null
  PICO_BACKEND=wasmgc ./target/debug/pico build --release --target wasm32-browser "$f" -o "$out/$name-gc" > /dev/null
  printf '%-28s %12s %12s %10s %10s\n' "$name" \
    "$(( $(stat -c %s "$out/$name-lin.wasm") / 1024 ))" \
    "$(( $(stat -c %s "$out/$name-gc.wasm") / 1024 ))" \
    "$(best "$out/$name-lin.js")" "$(best "$out/$name-gc.js")"
done
rm -rf "$out"
