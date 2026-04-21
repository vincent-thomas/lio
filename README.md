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

## Documentation

Project documentation lives in the mdBook under [`docs/`](docs/).

- Build locally: `mdbook build`
- Serve locally: `mdbook serve`
- API docs: [docs.rs/lio](https://docs.rs/lio)
- Crate: [crates.io/crates/lio](https://crates.io/crates/lio)

## License

MIT.
