#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
cd "$repo_root"

cargo build --release -p busybox --bin busybox >/dev/null

busybox_bin="$repo_root/target/release/busybox"

commands=(
  "$busybox_bin pwd"
  "$busybox_bin rg use Cargo.toml"
  "$busybox_bin jq . Cargo.toml"
)

if command -v rg >/dev/null 2>&1; then
  commands+=("rg use Cargo.toml")
fi

if command -v jq >/dev/null 2>&1; then
  commands+=("jq . Cargo.toml")
fi

hyperfine_args=(
  -N
  -w 10
  -m 50
  --ignore-failure
)

printf 'Profiling startup-oriented commands from %s\n' "$repo_root"
printf 'Using %s\n' "$busybox_bin"

exec hyperfine "${hyperfine_args[@]}" "${commands[@]}"
