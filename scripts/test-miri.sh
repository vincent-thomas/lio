#!/usr/bin/env bash
# Runs lio's core bookkeeping tests under Miri: the ones that drive the
# scripted backend, never a real one, since Miri cannot make the syscalls.
# Tests that need a real backend are `#[cfg_attr(miri, ignore)]`. Extra
# MIRIFLAGS (a borrow model, a seed) are passed through, as are extra nextest
# arguments (such as `--partition hash:1/2`). Needs cargo-nextest.

set -euo pipefail

# Proptest reads its saved regression seeds from disk.
export MIRIFLAGS="-Zmiri-disable-isolation ${MIRIFLAGS:-}"
export PROPTEST_CASES="${PROPTEST_CASES:-4}"

core='slab|backend::store|backend::testing|registration|time::clock|driver'
cargo +nightly miri nextest run -p lio --lib --test cancellation --test capacity \
  -E "(kind(lib) & test(/^(${core})::/)) | binary(cancellation) | binary(capacity)" \
  "$@"
