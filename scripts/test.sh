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
echo "==> all checks passed"
