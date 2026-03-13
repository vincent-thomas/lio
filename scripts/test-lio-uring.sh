#!/usr/bin/env bash
set -euo pipefail

if [[ "$(uname -s)" != "Linux" ]]; then
  echo "lio-uring tests can only run on Linux"
  exit 0
fi

RUST_BACKTRACE=1 cargo test -p lio-uring --release
