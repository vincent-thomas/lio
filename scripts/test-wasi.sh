#!/usr/bin/env bash
# Tests on WASI, under wasmtime.
#
# 1. lio's own tests that build for wasm32-wasip3: the library tests, which
#    include the WASI backend's (lio/src/backend/impls/wasi/tests.rs), and
#    the integration tests that do not build fixtures from std descriptors.
# 2. The lio-wasi examples, which assert what each operation did, and
#    examples/wasi, which drives the backend through lio's own API.
#
# A failed assertion aborts: wasm32-wasip3 does not unwind. Tests that catch
# a panic are ignored there (`not(panic = "unwind")`).
#
# Needs the wasm32-wasip3 Rust target and wasmtime (both from mise).

set -euo pipefail

target="wasm32-wasip3"
cargo build -p lio-wasi --examples --target "${target}"
cargo build -p lio-wasi-example --target "${target}"

out="${CARGO_TARGET_DIR:-target}/${target}/debug/examples"
scratch="$(mktemp -d)"
trap 'rm -rf "${scratch}"' EXIT

# The integration tests that build for WASI. The others share fixtures that
# turn std files and sockets into resources, which WASI cannot do.
tests=(callbacks capacity ds_cluster global model net_socket_datagram net_udp)

mkdir -p "${scratch}/tests/tmp"
echo "--- lio tests"
CARGO_TARGET_WASM32_WASIP3_RUNNER="wasmtime run -S p3 -S inherit-network --dir ${scratch}/tests::/ --env TMPDIR=/tmp" \
  cargo test -p lio --features high --target "${target}" --lib \
  "${tests[@]/#/--test=}"

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
wasmtime run -S p3 -S inherit-network --dir "${scratch}/lio::/" \
  "${out}/../lio-wasi-example.wasm" </dev/null
if [ -n "$(ls -A "${scratch}/lio")" ]; then
  echo "the lio example left files behind:"
  ls -A "${scratch}/lio"
  exit 1
fi

echo "wasi: every example passed."
