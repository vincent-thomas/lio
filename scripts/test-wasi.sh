#!/usr/bin/env bash
# Runtime checks for WASI. lio-wasi's examples assert what each operation
# did, and examples/wasi drives lio's WASI backend through lio's own API.
# This builds them as WebAssembly components and runs every one under
# wasmtime. A failed assertion traps, which fails the run.
#
# Needs the wasm32-wasip3 Rust target and wasmtime (both from mise).

set -euo pipefail

target="wasm32-wasip3"
cargo build -p lio-wasi --examples --target "${target}"
cargo build -p lio-wasi-example --target "${target}"

out="${CARGO_TARGET_DIR:-target}/${target}/debug/examples"
scratch="$(mktemp -d)"
trap 'rm -rf "${scratch}"' EXIT

run() {
  local name="$1"
  shift
  echo "--- ${name}"
  wasmtime run -S p3 "$@" "${out}/${name}.wasm"
}

printf 'hello' | run stdin_count
printf 'through the host\n' | run pipe
run timer </dev/null

mkdir "${scratch}/fs"
run fs_ops --dir "${scratch}/fs::/" </dev/null
if [ -n "$(ls -A "${scratch}/fs")" ]; then
  echo "fs_ops left files behind:"
  ls -A "${scratch}/fs"
  exit 1
fi

mkdir "${scratch}/read"
printf 'twelve bytes' >"${scratch}/read/file.txt"
echo "--- open_read"
read_out="$(wasmtime run -S p3 --dir "${scratch}/read::/" \
  "${out}/open_read.wasm" file.txt </dev/null)"
echo "${read_out}"
grep -q '^file.txt: 12 bytes$' <<<"${read_out}"

run sockets -S inherit-network </dev/null

mkdir "${scratch}/lio"
echo "--- lio backend"
wasmtime run -S p3 --dir "${scratch}/lio::/" \
  "${out}/../lio-wasi-example.wasm" </dev/null
if [ -n "$(ls -A "${scratch}/lio")" ]; then
  echo "the lio example left files behind:"
  ls -A "${scratch}/lio"
  exit 1
fi

echo "wasi: every example passed."
