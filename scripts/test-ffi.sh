#!/usr/bin/env bash

set -e

make cbuild
cargo test -p lio --features unstable_ffi --release --test ffi
