# lio

`lio` is a manually driven asynchronous I/O engine.

If you are new to the crate, the safest starting assumption is:

`lio` is not trying to be "another async runtime." It is trying to give you a direct, typed, manually driven way to work with asynchronous OS I/O.

The shortest accurate description of the crate is:

- `Lio` is the driver
- `lio::api` is the typed operation surface
- your code decides when progress happens

That combination is what makes `lio` feel different from a typical async runtime. You do not hand work to a hidden scheduler and wait for it to take over. You build operations explicitly, bind them to a driver explicitly, and drive I/O progress explicitly.

Nothing in `lio` assumes Tokio, async-std, or a custom executor. The library gives you a portable way to talk to platform I/O backends while keeping scheduling decisions in your own code.

## The Basic Idea

Most of the crate can be understood from one simple loop:

1. construct an operation
2. attach it to a `Lio`
3. choose how the result should come back
4. keep driving `Lio` until the result arrives

The first half of this book explains that loop from the outside in. The second half explains why the internals are shaped the way they are.

This book is intentionally **explaining documentation**.

It is organized as an introduction first and a deeper explanation second.

The first chapters answer:

1. what `lio` gives you direct control over
2. what one complete operation looks like in real code

The later chapters answer:

1. why the low-level API is shaped the way it is
2. how ownership and completion fit into that model
3. where platform boundaries and repository structure matter

## How This Book is Structured

This book follows a deliberate non-linear path:

1. **Introduction** (this page) - what is `lio`?
2. **Getting Started** - a complete working example you can build from
3. **The Core Model** - a deep dive into *why* the library is designed this way
   - Driving Operations
   - Resources and Buffers
   - Completion Models
   - OpModel (the deepest internal layer)
   - API Reference vs Explaining Documentation
4. **What lio Lets You Control** - zoom back out to see the full control surface
5. **Platform Support** and **Project Layout** - practical boundaries and context

This structure intentionally dives deep into the *how and why* before presenting the full *what*. The reason: once you understand the core model, the control surface makes much more sense. You'll understand not just what operations exist, but why they have the shape they do.

If you prefer a different reading order:
- For a quick overview first: read Introduction → Getting Started → What lio Lets You Control → (then dive into The Core Model)
- For the intended deep understanding: follow the chapter order as written

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

The next chapter is Getting Started. It walks through one complete operation first, so the broader map of the crate has something concrete to build on.
