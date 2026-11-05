#!/usr/bin/env bash
# Packages editors/vscode as a .vsix with Microsoft's vsce (the package the Marketplace gets),
# its npm dependencies installed first from the lockfile, and, with --install, installs it with
# VS Code's CLI (`code`, or $CODE). Needs Node.js and npm.
# Usage: scripts/vscode-package.sh [--install]   → target/vscode/pico-lang-<version>.vsix
set -euo pipefail
cd "$(dirname "$0")/.."
out="$PWD/target/vscode"
mkdir -p "$out"
cd editors/vscode
npm ci --omit=dev --no-audit --no-fund --silent
read -r name ver < <(node -p 'const p = require("./package.json"); `${p.name} ${p.version}`')
vsix="$out/$name-$ver.vsix"
npx --yes @vscode/vsce@3.9.2 package --out "$vsix" >&2
echo "$vsix"
if [ "${1:-}" = "--install" ]; then
  "${CODE:-code}" --install-extension "$vsix" --force
fi
