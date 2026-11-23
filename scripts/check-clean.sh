#!/usr/bin/env bash
# A build script that reruns every time rebuilds everything above it on
# every cargo invocation. The second check must compile nothing.
#
# Linux flags match tasks.check: --workspace --all-targets --all-features.

set -euo pipefail

case "$(uname -s)" in
  Linux) ;;
  *)
    echo "check-clean applies to Linux."
    exit 0
    ;;
esac

log="$(mktemp)"
trap 'rm -f "${log}"' EXIT

if ! CARGO_TERM_COLOR=never cargo check --verbose --workspace --all-targets --all-features >"${log}" 2>&1; then
  cat "${log}"
  exit 1
fi

if grep -E '^[[:space:]]+(Compiling|Checking) ' "${log}"; then
  echo "::error::these crates rebuilt with nothing changed; cargo's reasons:"
  grep -E '^[[:space:]]+Dirty ' "${log}" || true
  exit 1
fi

echo "second check compiled nothing"
