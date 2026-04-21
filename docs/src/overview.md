# What lio Lets You Control

The source code shows `lio` as a library for controlling **submission and completion of operating-system I/O operations**.

At the user level, that means `lio` gives you control over the boundary between your program and the OS. The point of this chapter is breadth, not depth. It names that control surface before later chapters show how to structure code around it.

## 1. Operation scheduling

`Lio` is the driver. It owns the backend, tracks in-flight work, tracks timers, and turns kernel completions into typed Rust results.

You decide when to drive it:

- `try_run()` for a non-blocking poll
- `run()` to wait until something completes
- `run_timeout()` to block for at most a chosen duration

That choice is one of the defining properties of `lio`: it does not hide the event loop from you.

If you want one sentence that separates `lio` from a typical async runtime, it is this: your code decides when I/O progress is driven.

## 2. File descriptor and handle lifetime

The `api::resource::Resource` type is the shared handle abstraction in the library.

It lets you:

- hold files, sockets, and standard streams
- clone references cheaply
- move resources across operations
- let cleanup happen automatically when the last owner drops

Most of the public API is phrased in terms of `&impl AsResource`, which means operations work against any wrapper that can expose an underlying resource.

That is the first sign that `lio` treats ownership as part of the API surface, not as an internal implementation detail.

## 3. Low-level I/O operations

The low-level `lio::api` module is the real control surface of the crate.

From source code, the active operation families are:

- timing: `nop`, `sleep`, `interval`
- socket lifecycle: `socket`, `bind`, `listen`, `accept`, `connect`, `shutdown`
- byte movement: `read`, `read_at`, `write`, `write_at`, `recv`, `send`
- path-relative filesystem work: `openat`, `statat`, `readdir`, `unlinkat`, `renameat`, `mkdirat`, `linkat`, `readlinkat`, `getcwd`
- process launch on Unix: `spawn`

These are deliberately close to OS operations. `lio` is not trying to erase the syscall model. It is trying to make that model typed, portable where possible, and asynchronous.

That syscall-shaped surface is one of the library's strongest design decisions. It keeps capability, cost, and sequencing visible.

## 4. Completion style

The same operation can be consumed in different ways.

From `api::io::Io`, the code supports:

- `.await`
- `.when_done(...)`
- `.send()` and `.send_with(...)`

This is not three different APIs. It is one operation model with three completion strategies.

That separation matters because application structure varies more than operation structure does.

## 5. Buffer ownership

For reads, writes, sends, and receives, `lio` moves the buffer into the operation and returns it when the operation completes.

That gives the library control over:

- memory lifetime while the kernel may still access the buffer
- zero-copy ownership transfer at the Rust level
- buffer reuse by the caller after completion

This design is central to `lio`. If you treat buffer ownership as an implementation detail, the library will feel strange. If you treat ownership as part of the API contract, the design becomes straightforward.

## 6. Backend choice without backend-shaped application code

`Lio::new()` picks a backend for the current platform:

- Linux: `io_uring`
- BSD/macOS family: polling/kqueue-based backend

The code is structured so application code talks to `Lio`, `Io<T>`, and `Resource`, not to backend-specific details. The backend matters for performance and capabilities, but it is not supposed to leak into normal application structure.

## Higher-level wrappers

There are two distinct layers in the repository.

The first is the low-level API in `lio::api`. That is the foundation.

The second is a thinner convenience layer:

- `lio::io` adds higher-level composed operations like `copy` and `copy_n`
- `lio::net` adds typed socket wrappers such as `Socket`, `TcpListener`, and `TcpSocket` behind the `high` feature

One detail from the source tree matters here: there is filesystem and process wrapper code in the repository, but those modules are not re-exported from the crate root. That means the low-level API remains the primary public way to control filesystem and process operations.

## What to carry forward

If you only keep four ideas from this chapter, keep these:

- `Lio` is an explicit driver
- `lio::api` is the main control surface
- ownership of resources and buffers is part of the API, not an implementation detail
- higher-level wrappers exist, but they sit on top of the low-level model

The next chapter stops listing capabilities and follows one operation from submission to completion. That is where these abstract control points become concrete.
