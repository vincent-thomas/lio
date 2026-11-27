#!/usr/bin/env bash
# Times the WASI 0.3 workloads in `guest/` on three hosts:
#
#   lio          this crate: I/O on lio, one thread
#   wasi-async   wasmtime-wasi as an embedder gets it: file I/O on tokio's
#                blocking pool
#   wasi-inline  wasmtime-wasi with `allow_blocking_current_thread`, as
#                `wasmtime run` sets it: file I/O blocks the calling thread
#
# All three run the same engine build. Times are measured inside the guest,
# so compiling the component is not in them.
#
#   ./bench.sh [REPEATS]

set -euo pipefail
cd "$(dirname "$0")"
repeats="${1:-5}"

cargo build --release --features baseline
(cd guest && cargo build --release --target wasm32-wasip3)

data="target/bench-data"
if [ ! -f "${data}/big.bin" ]; then
  mkdir -p "${data}/many"
  dd if=/dev/urandom of="${data}/big.bin" bs=1048576 count=256 2>/dev/null
  head -c 4096 /dev/urandom >"${data}/small.txt"
  (cd "${data}/many" && for i in $(seq 1 20000); do : >"entry-${i}.txt"; done)
fi

guest="guest/target/wasm32-wasip3/release/bench-guest.wasm"
workloads=(
  "read"
  "write 268435456"
  "stat 20000"
  "open 20000"
  "nofollow 20000"
  "small 20000"
  "list"
  "tcp 268435456"
  "burst 19200"
  "fan 300"
  "copy"
  "echo 20000"
  "conns 2000"
  "churn 5000"
  "smallw 20000"
)
hosts=(
  "lio:target/release/lio-wasi-run"
  "wasi-async:target/release/baseline-run"
  "wasi-inline:target/release/baseline-run --blocking"
)

for workload in "${workloads[@]}"; do
  for _ in $(seq 1 "${repeats}"); do
    for host in "${hosts[@]}"; do
      name="${host%%:*}"
      # shellcheck disable=SC2086
      line="$(${host#*:} --inherit-network --dir "${data}::/" "${guest}" ${workload})"
      echo "${name} ${line}"
    done
  done
done
