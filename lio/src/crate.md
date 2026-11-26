# `lio`

`lio` is a manually driven asynchronous I/O library.

Its central abstraction is [`Lio`], a driver that owns backend state, in-flight operations, and timers. The crate exposes typed operations in [`api`] and lets you choose how results are consumed: with `await`, callbacks, or channels.

## The mental model

`lio` is best understood as an I/O engine, not as a full async runtime.

You:

- create a `Lio`
- build operations such as `api::read`, `api::write`, `api::openat`, `api::connect`, or `api::sleep`
- bind them to a driver with `.with_lio(&lio)` or a scoped thread-local global installation
- drive progress with `Lio::try_run`, `Lio::run`, or `Lio::run_timeout`
- consume typed results when completions arrive

That is the center of the crate. Everything else makes more sense once you see `lio` as explicit submission plus explicit progress.

## What you can control

From the current public source, `lio` lets you control:

- timers and no-op wakeups
- socket creation, binding, listening, accepting, connecting, sending, receiving, and shutdown
- buffered file and socket reads and writes
- path-relative filesystem operations such as open, stat, directory iteration, rename, unlink, mkdir, link, and readlink
- Unix process spawning
- higher-level TCP wrappers through `lio::net` with the `high` feature

## Buffer and resource design

Two ideas shape most of the API:

- [`api::resource::Resource`] is the shared wrapper around OS resources
- I/O buffers move into operations and come back on completion

That ownership model is how `lio` keeps in-flight I/O memory-safe without hiding lifecycle details behind background tasks or borrowed buffers.

## Supported surface

Callers depend on [`Lio`], [`Io`](api::io::Io), and [`api`]. That is the supported surface: the driver, the operation handle, and the syscall-shaped operations.

[`backend`] is for people who implement a backend. It can change between releases. Do not build application code on it.

`IoBackend::drain` is a checked promise that the backend no longer stores a caller buffer address. The type system does not prove that the kernel has released that address. The driver keeps the operation model until drain says the backend holds nothing.

## API reference versus explanation

Rustdoc is the API reference.

It is the right place to look up exact function signatures, trait bounds, and per-item behavior. Explanatory material should stay focused on the design of the library: why there is a driver, why the API is syscall-shaped, and how to structure code around explicit progress.
