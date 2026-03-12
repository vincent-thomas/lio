#!/usr/bin/env bash

set -e

cargo fmt --check
cargo clippy --all-features
cargo deny check
cargo hack check --feature-powerset --lib --tests -p lio
