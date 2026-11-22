#!/usr/bin/env bash

set -e

cargo rustc -p lio --crate-type dylib,staticlib --features unstable_ffi --release
cargo test -p lio --features unstable_ffi --release --test ffi
