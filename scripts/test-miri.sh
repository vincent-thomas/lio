#!/usr/bin/env bash
# Runs lio's core bookkeeping tests under Miri: the ones that drive the
# scripted backend, never a real one, since Miri cannot make the syscalls.
# Tests that need a real backend are `#[cfg_attr(miri, ignore)]`. Extra
# MIRIFLAGS (a borrow model, a seed) are passed through.
#
# SHARD=k/n (1-based k) runs one slice of the selected tests, in sorted
# order, so the deep workflow can split the suite across jobs.

set -euo pipefail

export MIRIFLAGS="-Zmiri-disable-isolation ${MIRIFLAGS:-}"
export PROPTEST_CASES="${PROPTEST_CASES:-4}"

list_tests() {
  cargo +nightly test -p lio "$@" -- --list \
    | sed -n 's/: \(test\|ignore\)$//p'
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

if ((${#selected[@]} > 0)); then
  mapfile -t selected < <(printf '%s\n' "${selected[@]}" | sort)
fi

if [[ -n "${SHARD:-}" && ${#selected[@]} -gt 0 ]]; then
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
  selected=()
  if ((${#shard[@]} > 0)); then
    selected=("${shard[@]}")
  fi
fi

if ((${#selected[@]} == 0)); then
  echo "No Miri tests selected"
  exit 0
fi

cargo +nightly miri test -p lio --lib --test cancellation --test capacity -- "${selected[@]}"
