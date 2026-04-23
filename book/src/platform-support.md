# Platform Support

`lio` aims for a portable programming model, not identical kernel features on every target.

The user-facing model does not change from target to target. Operations are still typed, progress is still explicit, and ownership is still part of the API. What changes is the backend work needed to make that model real on a given platform.

At the design level, `lio` is compatible with any target platform or operating system. The requirement is not that the crate must ship a backend for every target. The requirement is that some `IoBackend` implementation must exist for that target. If the workspace does not already provide one, a custom backend has to be built.

## Backends

The code selects backends this way:

- on Linux, `Lio::new()` uses the `io_uring` backend from the `lio-uring` crate
- on macOS and the BSD family, `Lio::new()` uses the polling backend built around platform event facilities

That split matters for implementation, but application structure stays the same:

- submit typed operations
- drive `Lio`
- receive typed completions

On a target without a built-in backend, the same model still applies. The missing piece is backend implementation, not a different way of writing `lio` code.

## What portability means here

Portability in `lio` means:

- the same high-level control flow across platforms
- the same operation types where the crate exposes them
- backend-specific machinery hidden behind `Lio`

It does **not** mean every OS exposes identical primitives or identical behavior at the kernel level. `lio` gives you a portable control model, not a guarantee that kernels behave identically.

## Features and surface area

A second portability question is about **surface area**.

Some parts of the repository are always central:

- `Lio`
- `lio::api`
- `lio::api::io`
- `lio::api::resource`
- `lio::time`

Some parts are conditional or narrower:

- `lio::net` is behind the `high` feature
- the crate root does not re-export the higher-level `fs` and `process` modules even though related code exists in the repository
- the C FFI is behind the `unstable_ffi` feature

When reading examples or designing documentation, distinguish:

- what the repository contains
- what the public crate root exposes

This book is written around the public crate surface.

## Threading model

`Lio` is intentionally single-thread-oriented.

The implementation uses `Rc<RefCell<...>>` internally and supports a thread-local global installation model. That points to the intended usage model:

- one `Lio` per thread or event loop context
- explicit ownership of where completions are driven
- scoped global installation when you want thread-local lookup
- no hidden cross-thread scheduler

If you need multi-threaded designs, compose them above `lio` rather than expecting `lio` itself to be the scheduler.

That does not mean you cannot use `lio` in a larger concurrent program. It means `lio` should usually be one component inside that program rather than the thing that defines the whole scheduling model.

## Practical guidance

When you want code that survives backend differences, write to these concepts:

- `Resource` instead of raw descriptors where possible
- `api::*` operations instead of backend calls
- explicit driver loops
- capability checks in your application layer when behavior truly differs by platform

That keeps the portable part of your program in the `lio` layer that is meant to stay stable across backend implementations.
