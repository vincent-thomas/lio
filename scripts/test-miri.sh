#!/usr/bin/env bash
# Runs lio's core bookkeeping tests under Miri: the ones that drive the
# scripted backend, never a real one, since Miri cannot make the syscalls.
# Tests that need a real backend are `#[cfg_attr(miri, ignore)]`.
# `mise run miri` runs this script.
#
# SEED, BORROWS, and extra MIRIFLAGS are added to the Miri flags.
# SHARD=k/n (1-based k) runs one slice of the selected tests, in sorted
# order, so the nightly workflow can split the suite across jobs.

set -euo pipefail

# Nightly sizes. A caller can set either variable to override them.
# Depth 2 is shallow on purpose: each ordering is a fresh driver.
export PROPTEST_CASES="${PROPTEST_CASES:-16}"
export LIO_LIFECYCLE_DEPTH="${LIO_LIFECYCLE_DEPTH:-2}"

# SEED and BORROWS are the nightly matrix. MIRIFLAGS still passes extra flags.
miri_flags=(-Zmiri-disable-isolation)
if [[ -n "${SEED:-}" ]]; then
  echo "Miri seed ${SEED}"
  miri_flags+=("-Zmiri-seed=${SEED}")
fi
if [[ -n "${BORROWS:-}" ]]; then
  miri_flags+=("${BORROWS}")
fi
if [[ -n "${MIRIFLAGS:-}" ]]; then
  # Split on spaces. The value is a list of Miri flags.
  # shellcheck disable=SC2206
  miri_flags+=(${MIRIFLAGS})
fi
export MIRIFLAGS="${miri_flags[*]}"
# Nightly warns on lints stable does not have yet.
export RUSTFLAGS=""

# `cargo test -- --list` prints `path::name: test` or `: ignore`. The match
# is bash, so it does not depend on GNU sed.
list_tests() {
  local line listing
  listing="$(cargo +nightly test -p lio "$@" -- --list)"
  while IFS= read -r line; do
    case "$line" in
      *": test" | *": ignore")
        printf '%s\n' "${line%: *}"
        ;;
    esac
  done <<< "$listing"
}

selected=()
lib_list="$(list_tests --lib)"
int_list="$(list_tests --test cancellation --test capacity)"
while IFS= read -r name; do
  [[ -n "$name" ]] || continue
  if [[ "$name" =~ ^(slab|backend::store|backend::testing|registration|time::clock|driver):: ]]; then
    selected+=("$name")
  fi
done <<< "$lib_list"
while IFS= read -r name; do
  [[ -n "$name" ]] || continue
  selected+=("$name")
done <<< "$int_list"

# A selector that matches nothing used to fail the nextest run. Exiting 0
# here would turn a broken filter into a green Miri job.
if ((${#selected[@]} == 0)); then
  echo "No Miri tests selected" >&2
  exit 1
fi

sorted=()
while IFS= read -r name; do
  sorted+=("$name")
done < <(printf '%s\n' "${selected[@]}" | sort)
selected=("${sorted[@]}")

if [[ -n "${SHARD:-}" ]]; then
  k="${SHARD%/*}"
  n="${SHARD#*/}"
  shard=()
  i=0
  for name in "${selected[@]}"; do
    if (( i % n == k - 1 )); then
      shard+=("$name")
    fi
    i=$((i + 1))
  done
  # This shard can be empty when n is larger than the suite. The suite
  # itself was non-empty, so that is not a broken selector.
  if ((${#shard[@]} == 0)); then
    echo "No Miri tests in shard ${SHARD}"
    exit 0
  fi
  selected=("${shard[@]}")
fi

# Filters are substrings and are ORed. `--exact` keeps `test_remove` from
# also running `test_remove_all`, including across the lib and integration
# binaries that share this argument list.
cargo +nightly miri test -p lio --lib --test cancellation --test capacity -- --exact "${selected[@]}"
