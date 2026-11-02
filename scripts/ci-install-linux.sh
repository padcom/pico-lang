#!/usr/bin/env bash
# CI (Linux arm64 runners): LLVM 23 with lld from apt.llvm.org, plus the amd64 cross glibc and
# qemu-user for running linux-x64 programs (`tests/cross.rs`, the linux-x64 golden job).
set -euo pipefail
wget -qO /tmp/llvm.sh https://apt.llvm.org/llvm.sh
sudo bash /tmp/llvm.sh 23
sudo apt-get install -y llvm-23-dev libpolly-23-dev libzstd-dev zlib1g-dev liblld-23-dev \
  libc6-amd64-cross qemu-user-static
