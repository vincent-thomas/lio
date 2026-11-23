#!/usr/bin/env bash
# debug_assert! is compiled out of release builds. CI rejects it.

set -euo pipefail

if ! hits=$(git grep -nE 'debug_assert(_eq|_ne)?!' -- '*.rs'); then
  exit 0
fi

while IFS=: read -r file line _; do
  echo "::error file=${file},line=${line}::debug_assert is compiled out of release builds; use assert! (or assert_eq!/assert_ne!) instead"
done <<< "${hits}"

echo "${hits}"
echo "Found debug_assert!. Replace each with assert!, assert_eq!, or assert_ne!."
exit 1
