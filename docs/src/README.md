# lio

`lio` is a manually driven asynchronous I/O engine.

The codebase exposes one central object, [`Lio`](https://docs.rs/lio/latest/lio/struct.Lio.html), and a set of typed operations in `lio::api`. You create a driver, submit operations to it, decide how completions are consumed, and decide when progress is driven. Nothing in `lio` assumes Tokio, async-std, or a custom executor. The library gives you a portable way to talk to platform I/O backends while keeping scheduling decisions in your own code.

This book is intentionally **explaining documentation**.

It is organized around the questions a new user actually has:

1. what `lio` gives you direct control over
2. what one complete operation looks like in real code
3. why the low-level API is shaped the way it is
4. where platform boundaries and repository structure matter

It does **not** try to duplicate API reference material. The exact signatures, trait impls, and item-level details belong in rustdoc on docs.rs.

If you are reading this book, use it to answer questions like:

- Why is there a `Lio` value at all?
- What does `.with_lio(&lio)` actually mean?
- Why do buffers move into operations and come back out?
- When should I use `.await`, callbacks, or channels?
- Which parts of the crate are stable concepts, and which are thin wrappers?

Use the API reference when you need:

- the exact signature of `api::openat`
- the return type of `api::recv`
- every method on `api::io::Receiver`
- the trait bounds for a buffer type

The next chapter names the control surface. After that, Getting Started follows one operation all the way through so the rest of the book has something concrete to build on.
