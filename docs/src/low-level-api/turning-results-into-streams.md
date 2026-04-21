# API Reference vs Explaining Documentation

This repository needs both kinds of documentation, but they should do different jobs.

## API reference

API reference answers item-level questions:

- What is the exact signature of `api::readdir`?
- What does `Receiver::recv_timeout` return?
- Which traits does `Resource` implement?
- Is `TcpListener::bind` gated behind a feature?

Rustdoc is the right home for that material because it is generated from the public code and stays anchored to real items. It answers "what exists?" and "what is the exact contract of this item?"

## Explaining documentation

Explaining documentation answers different questions:

- What kind of library is `lio`?
- Why does it expose a manually driven driver?
- Why do operations own buffers?
- When should I pick channels, callbacks, or `await`?
- Why is the low-level API the real center of the crate?

Those are design and workflow questions. They are about how to think with the library, not how to look up a symbol.

## What went wrong with the old shape

The earlier book structure leaned too far toward mechanism before model.

That makes readers reconstruct too much for themselves:

- they see mechanisms before they see purpose
- they see method families before they see control boundaries
- they have to infer which parts are reference and which parts are explanation

The source code suggests a clearer split.

## A better split for `lio`

For this project, the documentation split should be:

- **Book**: explain the architecture, the mental model, the operation families, and how to structure code around `Lio`
- **Rustdoc**: describe every public item precisely

That is why this book talks in terms of:

- drivers
- resources
- buffers
- completion styles
- backend portability
- public layers

instead of trying to restate every signature already present in rustdoc.

## How to use both together

The intended reading flow is:

1. read this book to understand the design
2. go to rustdoc when you need exact item-level detail
3. come back to the book when you need to re-evaluate structure or tradeoffs

If a reader understands the model but cannot remember a signature, rustdoc solves that. If a reader can find every item but still cannot explain why the crate is shaped this way, the book has not done its job.
