#!/usr/bin/env bash
# Every mutant of the core files listed in .cargo/mutants.toml.
# SHARD=k/n (0-based k, as cargo-mutants expects) runs one slice.

set -euo pipefail

export PROPTEST_CASES="${PROPTEST_CASES:-64}"
export LIO_LIFECYCLE_DEPTH="${LIO_LIFECYCLE_DEPTH:-4}"
export CARGO_PROFILE_DEV_DEBUG="${CARGO_PROFILE_DEV_DEBUG:-0}"

if [[ "$(uname -s)" == "Linux" ]]; then
  ulimit -v 8388608
fi

args=(--no-shuffle --jobs 2)
if [[ -n "${SHARD:-}" ]]; then
  args+=(--shard "${SHARD}")
fi

cargo mutants "${args[@]}"
