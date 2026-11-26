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

Callers depend on `Lio`, `Io`, and `api`. That is the supported surface.
`backend` is for people who implement a backend, and it can change. `drain`
is a checked promise that a backend holds no caller buffer address. The type
system does not prove that the kernel released that address.

## Benchmarks

```sh
mise run bench
```

runs the two Criterion benches in order:

```sh
cargo bench -p lio --bench bookkeeping
cargo bench -p lio --bench backends
```

`bookkeeping` is the driver with an in-memory backend: a batch round trip, the
same batch with no driver under it, cancellation, a 256-byte buffered read
the backend does not fill, a stream that rearms one registration, and the
timer wheel advanced by a tick. Those numbers leave out the kernel.

`backends` is one 64-byte UDP loopback transfer, repeated at the same batch
sizes, on every real backend this OS implements. io_uring is Linux, the
poller (epoll or kqueue) is Linux, macOS, and the BSDs, and IOCP is Windows.
Those numbers include the kernel. A backend this OS does not implement is not
compiled in. The module docs on `lio/benches/bookkeeping.rs` and
`lio/benches/backends.rs` say what each group includes.

Criterion flags go after `--`, for example
`cargo bench -p lio --bench bookkeeping -- --sample-size 100`.

## Documentation

- API docs: [docs.rs/lio](https://docs.rs/lio)
- Crate: [crates.io/crates/lio](https://crates.io/crates/lio)

## License

MIT.
