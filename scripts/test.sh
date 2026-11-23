#!/usr/bin/env bash
# The test suite is `mise run test`.

set -euo pipefail

cd "$(dirname "$0")/.."
exec mise run test
