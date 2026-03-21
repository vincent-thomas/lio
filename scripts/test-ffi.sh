#!/usr/bin/env bash

set -e

# On macOS, ensure libiconv is found by the linker
if [[ "$(uname -s)" == "Darwin" ]]; then
  # Find libiconv from nix store if available
  if command -v xcrun &> /dev/null; then
    SDK_PATH=$(xcrun --show-sdk-path 2>/dev/null || true)
    if [[ -n "$SDK_PATH" ]]; then
      export LIBRARY_PATH="${SDK_PATH}/usr/lib:${LIBRARY_PATH:-}"
    fi
  fi
fi

make cbuild
cargo test -p lio --features unstable_ffi --release --test ffi
