#!/usr/bin/env bash
set -euo pipefail

if [[ "$(uname -s)" != "Linux" ]]; then
  echo "lio-uring tests can only run on Linux"
  exit 0
fi

# Ensure kernel headers are available for lio-uring build
for dir in ${PATH//:/ }; do
  STORE_PATH="${dir%/bin}"
  if [[ -d "$STORE_PATH/include/linux" ]]; then
    export C_INCLUDE_PATH="${STORE_PATH}/include:${C_INCLUDE_PATH:-}"
    break
  fi
done

RUST_BACKTRACE=1 cargo test -p lio-uring --release
