#!/bin/sh
# Builds the release `pico`, installs it to ~/bin, and installs the VS Code extension.
set -e
cd "$(dirname "$0")"

# POSIX `.`, not `source`: /bin/sh is dash on Debian, which has no `source`.
. scripts/env.sh

cargo build -p pico-cli --release
cp target/release/pico ~/bin

scripts/vscode-package.sh --install
