#!/usr/bin/env bash

set -euo pipefail

# Linux g++ comes from conda:gxx in mise.toml. It finds its own libstdc++.
# macOS uses the image clang++. The test picks the compiler.
cargo rustc -p lio --crate-type dylib,staticlib --features unstable_ffi --release
cargo test -p lio --features unstable_ffi --release --test ffi
