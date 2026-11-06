#!/bin/sh
set -e
cd "$(dirname "$0")"

. scripts/env.sh

cargo build -p pico-cli --release
scripts/vscode-package.sh --install
