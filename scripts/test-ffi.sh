#!/usr/bin/env bash

set -euo pipefail

# mise installs clang++ on Linux for bindgen. That clang++ does not search
# GCC's library directory, so the C++ tests cannot find libstdc++ unless
# the linker is pointed at the g++ that ships it.
if [ "$(uname -s)" = Linux ] && command -v g++ >/dev/null 2>&1; then
  libstdcxx=$(g++ -print-file-name=libstdc++.so)
  libdir=$(dirname "$libstdcxx")
  if [ "$libdir" != "." ]; then
    export LIBRARY_PATH="${libdir}${LIBRARY_PATH:+:$LIBRARY_PATH}"
  fi
fi

cargo rustc -p lio --crate-type dylib,staticlib --features unstable_ffi --release
cargo test -p lio --features unstable_ffi --release --test ffi
