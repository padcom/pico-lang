#!/usr/bin/env bash
# CI (Linux arm64 runners): LLVM 23 with lld and clang's static libraries from apt.llvm.org,
# the static libraries release builds link (scripts/package.sh), plus the amd64 cross glibc and
# qemu-user for running linux-x64 programs (`tests/cross.rs`, the linux-x64 golden job).
set -euo pipefail
# (apt.llvm.org is sometimes briefly unreachable: retry instead of failing the job.)
echo 'Acquire::Retries "5";' | sudo tee /etc/apt/apt.conf.d/80-retries >/dev/null
wget --tries=5 --waitretry=15 --retry-connrefused --retry-on-http-error=429,500,502,503,504 \
  -qO /tmp/llvm.sh https://apt.llvm.org/llvm.sh
sudo bash /tmp/llvm.sh 23
sudo apt-get install -y llvm-23-dev libpolly-23-dev libzstd-dev zlib1g-dev liblld-23-dev libclang-23-dev \
  libffi-dev libxml2-dev libicu-dev liblzma-dev \
  libc6-amd64-cross qemu-user-static
