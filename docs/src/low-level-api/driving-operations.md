# Driving Operations

The central fact to understand about `lio` is this:

**submitting work and driving progress are separate responsibilities.**

## What “driving” means

When an operation is consumed, `lio` registers it and dispatches backend work. But completions are processed only when the driver runs.

That is why the public API on `Lio` is so small:

- `try_run()` polls without blocking
- `run()` blocks until progress is made
- `run_timeout()` blocks for a bounded amount of time

Everything else in the crate assumes you understand when those methods are called. If progress feels mysterious in a piece of `lio` code, this is usually the missing concept.

## The common loop

```rust,no_run
# use lio::{Lio, api};
# fn drive_until_done<T>(lio: &Lio, mut rx: api::Receiver<T>) -> T {
loop {
    if let Some(result) = rx.try_recv() {
        break result;
    }

    if lio.try_run().expect("poll") == 0 {
        lio.run().expect("wait");
    }
}
# }
```

This loop expresses the intended control flow:

- first, consume anything already ready
- then poll the backend without blocking
- if nothing happened, block until something does

The BusyBox example in the repository uses this pattern repeatedly because it matches a library that exposes the event loop directly instead of hiding it in a runtime.

## Why `lio` works this way

Many async libraries hide the driver inside a runtime. `lio` keeps it explicit because it is designed as an I/O engine, not as a full task scheduler.

That gives the caller direct control over:

- when blocking is allowed
- how much work to batch
- where integration points exist with other loops or schedulers

That explicit progress model comes first because the rest of the crate is built around it. Once you know who drives `Lio`, the ownership and completion APIs stop looking arbitrary.

## Consuming an operation starts it

Creating `api::read(...)` does not do I/O by itself.

The operation becomes active when you consume the `Io<T>`:

- by awaiting it
- by registering a callback
- by converting it into a receiver

That detail explains why `Io<T>` is marked `#[must_use]` in the source. A constructed operation that is never consumed never becomes live.

## Global versus explicit binding

There are two ways for an operation to know which driver should own it:

- call `.with_lio(&lio)`
- install a thread-local global driver and hold the returned guard

The explicit form is easier to reason about because the binding is visible at the call site. The global form fits code that already has a one-driver-per-thread architecture and wants less call-site noise.

## Driving more than one operation

The design becomes more interesting when you have several operations in flight, because the driver loop becomes the place where coordination happens.

You can:

- keep several receivers and poll them between `try_run()` calls
- send many completions into a shared channel
- mix callbacks and channel consumers

The driver does not care which completion style you choose. Its job is to move registered work forward and deliver typed results when they are ready.

## The habit to build

When working in `lio`, always ask two questions:

1. How is this operation being consumed?
2. Who is responsible for calling `Lio::run` or `Lio::try_run`?

If those answers are clear, the code is usually sound in structure. If they are vague, the code usually has a design problem.

The next chapter turns to the second major part of the model: ownership. `lio` makes resource and buffer lifetime visible on purpose.
