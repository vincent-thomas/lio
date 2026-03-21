#!/usr/bin/env bash

set -e

RELEASE_FLAG=""
if [[ "${1:-}" == "--release" ]]; then
    RELEASE_FLAG="--release"
fi

FEATURES=$(cargo metadata --no-deps --format-version 1 | jq -r '.packages[0].features | keys[]' | grep -v '^unstable_ffi$' | tr '\n' ' ')

flags=(-p lio --features "$FEATURES")

cargo nextest r "${flags[@]}" $RELEASE_FLAG

RUST_BACKTRACE=1 cargo test --doc $RELEASE_FLAG
