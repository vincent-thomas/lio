#!/usr/bin/env bash
# Create the GitHub release and publish lio-uring, then lio.
# Run `mise run release` first. The version is already committed.
# This task is the only one that should see the publish tokens.

set -euo pipefail

: "${GITHUB_TOKEN:?Set GITHUB_TOKEN to create the GitHub release.}"
: "${CARGO_REGISTRY_TOKEN:?Set CARGO_REGISTRY_TOKEN to publish to crates.io.}"

version="$(cargo metadata --no-deps --format-version 1 | jq -r '.packages[] | select(.name == "lio") | .version')"

gh release create "v${version}" --generate-notes

cargo publish -p lio-uring
sleep 30
cargo publish -p lio
