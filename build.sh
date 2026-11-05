#!/bin/sh

source scripts/env.sh

cargo build -p pico-cli --release
cp target/release/pico ~/bin

scripts/vscode-package.sh --install
