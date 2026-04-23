# What lio Lets You Control

The source code shows `lio` as a library for controlling **submission and completion of operating-system I/O operations**.

By this point in the book, the basic model should already be clear: `Lio` is the driver, operations are typed values, progress is explicit, and ownership is visible. This chapter zooms back out and names the main areas that model gives you control over.

If you want one sentence to carry through the whole book, use this one:

`lio` gives you typed operations, and it gives you the driver that moves them forward.

Everything else is a consequence of that.

Earlier chapters focused on the shape of one operation and the principles behind it. This chapter is different. It is a map of the territory. It is here so the reader can say "now that I understand the model, what kinds of things can I actually do with it?"

## 1. Progress

`Lio` is the driver. It owns the backend, tracks in-flight work, tracks timers, and turns kernel completions into typed Rust results.

You decide when to drive it:

- `try_run()` for a non-blocking poll
- `run()` to wait until something completes
- `run_timeout()` to block for at most a chosen duration

That choice is one of the defining properties of `lio`: it does not hide the event loop from you.

If you want one sentence that separates `lio` from a typical async runtime, it is this: your code decides when I/O progress is driven.

That control over progress is the first major part of the crate's design. The second is that the operations themselves are explicit and typed.

## 2. Operation Surface

The low-level `lio::api` module is the main control surface of the crate.

From source code, the active operation families are:

- timing: `nop`, `sleep`, `interval`
- socket lifecycle: `socket`, `bind`, `listen`, `accept`, `connect`, `shutdown`
- byte movement: `read`, `read_at`, `write`, `write_at`, `recv`, `send`
- path-relative filesystem work: `openat`, `statat`, `readdir`, `unlinkat`, `renameat`, `mkdirat`, `linkat`, `readlinkat`, `getcwd`
- process launch on Unix: `spawn`

These are deliberately close to OS operations. `lio` is not trying to replace the syscall model with an unrelated abstraction. It is trying to make that model typed, portable where possible, and asynchronous.

That syscall-shaped surface keeps three things visible:

- what the operation can actually do
- what it will probably cost
- what sequence of steps your program is asking the OS to perform

## 3. Ownership

`lio` makes ownership part of the user-facing model.

The `api::resource::Resource` type is the shared handle abstraction in the library. It lets you hold files, sockets, and standard streams with shared ownership and cheap cloning.

For byte I/O, buffers move into operations and come back on completion. That gives the crate control over:

- memory lifetime while the kernel may still touch the buffer
- reuse of the same allocation after completion
- explicit ownership boundaries instead of borrowed async state

This is one of the ideas new readers usually need to adjust to. Once it clicks, much of the API stops looking unusual.

## 4. Completion

The same operation can be consumed in different ways.

From `api::io::Io`, the code supports:

- `.await`
- `.when_done(...)`
- `.send()` and `.send_with(...)`

This is not three different APIs. It is one operation model with three completion strategies.

That separation matters because application structure varies more than operation structure does. A sleep, read, or connect operation does not need to be redesigned just because the surrounding program prefers `await`, callbacks, or channels.

## 5. Portability And Layers

`Lio::new()` picks a backend for the current platform:

- Linux: `io_uring`
- BSD/macOS family: polling/kqueue-based backend

The code is structured so application code talks to `Lio`, `Io<T>`, and `Resource`, not to backend-specific details. The backend matters for performance and capabilities, but it is not supposed to leak into normal application structure.

On top of that low-level surface, there is also a thinner convenience layer:

- `lio::io` adds higher-level composed operations like `copy` and `copy_n`
- `lio::net` adds typed socket wrappers such as `Socket`, `TcpListener`, and `TcpSocket` behind the `high` feature

One detail from the source tree matters here: there is filesystem and process wrapper code in the repository, but those modules are not re-exported from the crate root. That means the low-level API remains the primary public way to control filesystem and process operations.

That is an important distinction for readers of the codebase. The repository contains more than the crate root exposes, but the low-level API is still the public center of gravity.

## What to carry forward

If you only keep four ideas from this chapter, keep these:

- `Lio` is an explicit driver
- `lio::api` is the main control surface
- ownership of resources and buffers is part of the API, not an implementation detail
- higher-level wrappers exist, but they sit on top of the low-level model

The next chapters move from this broad control surface to the remaining boundaries around it: platform support and the structure of the repository itself.
