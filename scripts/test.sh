#!/usr/bin/env bash
# Full check: formatting, lints, tests. Must pass before every commit.
set -euo pipefail
cd "$(dirname "$0")/.."
source scripts/env.sh

echo "==> cargo fmt --check"
cargo fmt --all -- --check
echo "==> cargo clippy"
cargo clippy --workspace --all-targets --quiet -- -D warnings
echo "==> cargo test"
cargo test --workspace --quiet
# CI's WasmGC job (`golden (wasm32-browser)`): fixtures the WasmGC backend can't run yet are
# pending (tests/wasmgc-skip.txt, tests/browser-skip.txt). The glue runs under Node.js.
echo "==> golden tests on the WasmGC backend"
if ! command -v node >/dev/null 2>&1; then
  echo "error: node (Node.js) isn't on PATH; the WasmGC golden tests need it" >&2
  exit 1
fi
PICO_BACKEND=wasmgc PICO_TEST_TARGET=wasm32-browser cargo test --quiet -p pico-tests --test golden
echo "==> all checks passed"
