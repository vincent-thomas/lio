#!/usr/bin/env bash
# A mutant of a changed line in the core must make a test fail.
# .cargo/mutants.toml says which files count as core.
# A surviving mutant in a PR diff fails CI.

set -euo pipefail

base="${BASE:-${GITHUB_BASE_REF:-}}"
if [[ -z "${base}" ]]; then
  echo "Set BASE to the branch to diff against. CI sets GITHUB_BASE_REF." >&2
  exit 1
fi

git diff "origin/${base}...HEAD" > pr.diff

# A mutant is almost always caught within a few cases. The nightly search
# task runs the large searches. Debug info only slows each mutant rebuild.
export PROPTEST_CASES="${PROPTEST_CASES:-64}"
export LIO_LIFECYCLE_DEPTH="${LIO_LIFECYCLE_DEPTH:-4}"
export CARGO_PROFILE_DEV_DEBUG="${CARGO_PROFILE_DEV_DEBUG:-0}"

# A mutant can loop while allocating. Cap each process at 8 GiB so that
# test fails instead of taking the runner down.
if [[ "$(uname -s)" == "Linux" ]]; then
  ulimit -v 8388608
fi

cargo mutants --in-diff pr.diff --no-shuffle --jobs 2
