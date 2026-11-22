# lio

`lio` is a low-level I/O project centered on explicit control.

The main crate exposes syscall-shaped, non-blocking operations without imposing
an executor or runtime model. Instead of hiding submission and completion behind
a framework, `lio` lets the caller decide how work is driven, how completions
are consumed, and which backend is used underneath.

This repository contains:

- `lio`: the main cross-platform I/O crate
- `lio-uring`: the Linux `io_uring` backend crate
- `examples/busybox`: a larger example application built on top of `lio`

The project is still evolving, but the direction is stable: stay low-level,
preserve explicit ownership and control, and keep the abstraction close to the
operating system.

## Benchmarks

Two Criterion benches compare the runtime with the backends:

```sh
mise run bench
```

That runs, in order:

```sh
cargo bench -p lio --bench bookkeeping
cargo bench -p lio --bench backends
```

`bookkeeping` times scheduling, submission, completion routing, cancellation,
and buffer handling. Its backend completes operations in memory, so the
numbers leave out kernel I/O. `backends` times the same cached 4 KiB read on
every real backend this OS implements: `io_uring` on Linux, the poller
(epoll or kqueue) on Linux, macOS, and the BSDs, and IOCP on Windows. Those
numbers include the kernel. A backend this OS does not implement is not
compiled in; run that bench on a machine that has it. The module docs at the
top of `lio/benches/bookkeeping.rs` and `lio/benches/backends.rs` say what
each group includes.

Criterion flags go after `--`, for example
`cargo bench -p lio --bench bookkeeping -- --sample-size 100`.

## Documentation

- API docs: [docs.rs/lio](https://docs.rs/lio)
- Crate: [crates.io/crates/lio](https://crates.io/crates/lio)

## License

MIT.
