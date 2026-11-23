#!/usr/bin/env bash
# Bump lio and lio-uring, tag v<version>, and publish.
# BUMP is patch, minor, or major. The workflow runs `mise run test` first.
# GITHUB_TOKEN and CARGO_REGISTRY_TOKEN are used only by gh and cargo publish.

set -euo pipefail

bump="${BUMP:?Set BUMP to patch, minor, or major}"
case "${bump}" in
  patch | minor | major) ;;
  *)
    echo "BUMP must be patch, minor, or major." >&2
    exit 1
    ;;
esac

github_token="${GITHUB_TOKEN:-}"
registry_token="${CARGO_REGISTRY_TOKEN:-}"
unset GITHUB_TOKEN
unset CARGO_REGISTRY_TOKEN

if [[ -z "${github_token}" || -z "${registry_token}" ]]; then
  echo "GITHUB_TOKEN and CARGO_REGISTRY_TOKEN are required to publish." >&2
  exit 1
fi

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

GITHUB_TOKEN="${github_token}" gh release create "v${version}" --generate-notes

CARGO_REGISTRY_TOKEN="${registry_token}" cargo publish -p lio-uring
sleep 30
CARGO_REGISTRY_TOKEN="${registry_token}" cargo publish -p lio
