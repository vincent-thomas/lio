#!/usr/bin/env bash
# Bump lio and lio-uring, commit, tag, and push.
# BUMP is patch, minor, or major. The workflow runs `mise run test` first.
# This task does not publish. `mise run publish` does that, with the tokens.

set -euo pipefail

bump="${BUMP:?Set BUMP to patch, minor, or major}"
case "${bump}" in
  patch | minor | major) ;;
  *)
    echo "BUMP must be patch, minor, or major." >&2
    exit 1
    ;;
esac

cargo install cargo-edit --locked

git config user.name "github-actions[bot]"
git config user.email "github-actions[bot]@users.noreply.github.com"

old="$(cargo metadata --no-deps --format-version 1 | jq -r '.packages[] | select(.name == "lio-uring") | .version')"

cargo set-version -p lio-uring --bump "${bump}"
cargo set-version -p lio --bump "${bump}"

version="$(cargo metadata --no-deps --format-version 1 | jq -r '.packages[] | select(.name == "lio") | .version')"

sed "/^lio-uring = /s/version = \"${old}\"/version = \"${version}\"/" lio/Cargo.toml > tmp && mv tmp lio/Cargo.toml

git add lio/Cargo.toml lio-uring/Cargo.toml Cargo.lock
git commit -m "release: v${version}"
git tag "v${version}"
git push origin main
git push origin "v${version}"
