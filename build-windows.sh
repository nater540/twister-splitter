#!/usr/bin/env bash
#
# Cross-compile twister-splitter to a self-contained Windows x86_64 .exe using
# Docker + cargo-xwin (MSVC target). Requires a running Docker daemon (e.g. start
# OrbStack / Docker Desktop).
#
# Output: ./dist/twister-splitter.exe (opens the desktop GUI when double-clicked)
set -euo pipefail
cd "$(dirname "$0")"

docker build -f Dockerfile.windows --target export --output "type=local,dest=./dist" .

echo
echo "Built ./dist/twister-splitter.exe ($(du -h dist/twister-splitter.exe | cut -f1))"
