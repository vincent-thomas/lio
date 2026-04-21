# Project Layout

Once the public model is clear, the repository layout becomes much easier to read. The source tree separates the public operation surface, the driver and backend machinery, and the shared test infrastructure in a fairly direct way.

## `lio`

This is the main crate and the only crate most users need to care about.

Its public center is:

- `Lio`
- `lio::api`
- `lio::api::io`
- `lio::api::resource`
- `lio::io`
- `lio::time`
- `lio::net` behind the `high` feature

Internally, this crate is split between:

- operation models in `src/api`
- runtime/driver machinery in `src/lio.rs`, `src/registration.rs`, and `src/backend`
- time management in `src/time`
- utility ownership types such as buffers and slabs

That tells you how the crate is designed: the public API is typed operations, while the internal engine is registration, backend dispatch, and completion processing.

A productive reading order is from the outside in:

- the crate root shows what users are meant to touch
- `src/api` shows the typed operation surface
- `src/backend` shows the execution substrate
- `src/lio.rs` shows how the driver ties registration, backend work, and timers together

## `lio-uring`

This crate is the Linux backend layer.

It wraps `liburing` and exposes the lower-level machinery needed for the Linux implementation of `Lio`. Unless you are working on backend behavior, it is best treated as implementation detail.

## `examples/busybox`

This example matters because it shows the library at application scale.

It is not a demo of isolated calls. It is a larger application that uses:

- a long-lived `Lio`
- manual driver loops
- low-level filesystem operations
- path-relative handling
- callback and channel-based completion styles

If you want to see how the explicit driver model feels in a larger codebase, this directory is more revealing than a collection of tiny examples.

## `examples`

The smaller examples show focused usage patterns:

- a minimal HTTP client
- sleep timing behavior
- C FFI integration

They are a good place to confirm one idea at a time: a sleep loop, an I/O submission pattern, or an FFI boundary.

## `lio-test`

This crate holds shared testing support used across the workspace.

Its role is narrow:

- backend contract macros live there
- `OpModel` contract-test support lives there
- test infrastructure stays separate from the runtime crate's core API surface

That separation keeps reusable test machinery from looking like part of the public runtime model.

## What this means for documentation

The project layout suggests a documentation strategy:

- explain the main crate first
- treat the backend crate as implementation detail unless the reader is extending `lio`
- use examples to illustrate control flow
- keep API reference attached to public items, not duplicated in the book

That structure matches the code better than a book organized around isolated mechanism names. It also gives readers a practical way to navigate the repo once the conceptual model is in place.
