#!/usr/bin/env bash

set -e

# On macOS, ensure libiconv is found by the linker
if [[ "$(uname -s)" == "Darwin" ]]; then
  if command -v xcrun &> /dev/null; then
    SDK_PATH=$(xcrun --show-sdk-path 2>/dev/null || true)
    if [[ -n "$SDK_PATH" ]]; then
      export LIBRARY_PATH="${SDK_PATH}/usr/lib:${LIBRARY_PATH:-}"
    fi
  fi
fi

# On Linux, ensure kernel headers are available for lio-uring build
if [[ "$(uname -s)" == "Linux" ]]; then
  # Find linuxHeaders from PATH (nix puts binaries in PATH, headers in same store path)
  for dir in ${PATH//:/ }; do
    STORE_PATH="${dir%/bin}"
    if [[ -d "$STORE_PATH/include/linux" ]]; then
      export C_INCLUDE_PATH="${STORE_PATH}/include:${C_INCLUDE_PATH:-}"
      break
    fi
  done
fi

make cbuild
cargo test -p lio --features unstable_ffi --release --test ffi
