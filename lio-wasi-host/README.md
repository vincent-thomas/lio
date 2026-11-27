# lio-wasi-host

A WASI 0.3 host for wasmtime whose I/O runs on lio. An experiment: it is not
a workspace member and has its own lockfile.

wasmtime runs the guest and the component model's async machinery. This
crate implements `wasi:cli/command@0.3` (cli, clocks, filesystem, sockets,
random) and awaits a lio operation wherever a WASI function can wait.

```bash
cargo build --release
```

```bash
target/release/lio-wasi-run --dir .::/ --inherit-network guest.wasm
```

## How it works

- `block_on` polls the store's future on the calling thread and blocks in
  `Lio::run` while it is waiting. Everything runs on that thread: lio's
  futures are not `Send`, so each is wrapped in a `Local` (`src/local.rs`),
  which panics if it is touched from any other thread.
- An operation gets one chance to finish inside the host call that started
  it: after its first poll the driver takes in what has completed, without
  waiting. The guest then never sees it block.
- A guest's read gives lio the guest's own buffer (`src/lend.rs`), so the
  kernel writes into guest memory and nothing is copied. `Engine` turns
  memory relocation off, and `Store` does not free or recycle a guest's
  memory until lio has handed every such buffer back. A cancelled read is
  not reported over until then either, and reports any bytes that arrived
  first.
- A guest's write is the same the other way: the kernel reads from the
  guest's memory. A write in flight is not cancelled, and the guest's
  write is not over until it is. Only a store being torn down stops one.
- A stream the guest got from the host and passed to another host function
  has no guest memory at either end, so it keeps a host buffer and one copy.
  wasmtime does not say which kind a write is, so `Origin` in
  `src/stream.rs` asks it to move one byte into a buffer that keeps none and
  watches which way wasmtime does it. That leans on wasmtime's internals:
  `wasmtime` should be pinned to an exact version, and `bench-guest splice`
  with `--stats` checks it.

## What does not go through lio

lio has no operation for these, so they are direct system calls:

- `stat` and `stat-at` (lio's `FileStat` has no timestamps), `metadata-hash`,
  `is-same-object`, `set-size`, `set-times`
- creating, binding and configuring sockets, and `listen`
- name resolution, which blocks the thread

`sync-data` is a full `fsync`, `advise` does nothing, and an append reads
the file's size before each write.

## Not a sandbox

Paths are checked by their text. Every `..` is resolved against the names
before it, and none reaches the system, so a path cannot step out by way of
a symbolic link. A link a guest makes stores a relative target with no
`..`. A link that was already inside a preopened directory and points
outside it is still followed.

## Benchmarks

`./bench.sh [REPEATS]` times the workloads in `guest/` on this host and on
`wasmtime-wasi` in its two modes, with the same engine build.

The WIT in `wit/` is copied from wasmtime v51.0.0.
