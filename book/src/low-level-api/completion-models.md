# Completion Models

Once progress and ownership are clear, the next layer is completion style.

One of `lio`'s stronger design choices is that completing an operation is a separate concern from defining the operation itself.

The same `api::read`, `api::connect`, or `api::sleep` can be written once and then consumed in different ways. That matters because application structure varies more than syscall structure does. Some programs want straight-line async code. Some already have a manual event loop. Some want to route completions through shared coordination code.

That separation is one of the cleanest parts of the crate's design. The operation says what work should happen. The completion model says how your program wants to receive the result.

## One operation, three ways to receive the result

From `api::io::Io<T>`, the source exposes three primary completion styles:

- `await`
- callback with `.when_done(...)`
- channel delivery with `.send()` or `.send_with(...)`

These are not separate subsystems with different semantics. They are different front ends over the same operation model, which means you can often change completion style without changing the operation itself.

## 1. `await`

Use `.await` when you already have an async context and want the clearest linear code.

```rust,no_run,edition2024
# extern crate lio;
use lio::{Lio, api};
use lio::api::resource::Resource;

async fn write_once(lio: &Lio) -> std::io::Result<()> {
    let stdout = Resource::stdout();
    let (result, _buf) = api::write(&stdout, b"hello\n".to_vec())
        .with_lio(lio)
        .await;
    result?;
    Ok(())
}
```

The key constraint is that `await` changes syntax, not who drives progress. A `lio` operation can implement `IntoFuture` and still depend on `Lio` being driven somewhere else. The library does not stop being manually driven just because one caller chooses async syntax.

## 2. Callbacks

Use `.when_done(...)` when completion should trigger follow-up work directly.

This style fits code that already has explicit state and wants to keep that state in one place. It is often the most natural choice when:

- you are already building a manual event loop
- you want to fan many operations into one shared state machine
- completion should be pushed, not polled

Callbacks are less about convenience than about shape. They let you keep control flow in the same evented style as the rest of the program.

## 3. Channels

Use `.send()` or `.send_with(...)` when you want explicit result ownership and flexible coordination.

This is often the easiest style to integrate with a hand-written driver loop because the control flow stays explicit:

- start operations
- drive `Lio`
- receive results from channels

The repository uses this style heavily in tests and examples for exactly that reason. It keeps progress, waiting, and result handling visible instead of spreading them across tasks or callbacks.

## Choosing between them

A reasonable default is:

- `await` for straight-line async code
- callbacks for highly evented code
- channels for manual loops, batching, and cross-component coordination

The better question is not which style is most modern. It is which style matches the structure you already have. In `lio`, operation definitions stay mostly the same while completion style adapts to the surrounding program.

That is the right way to think about this layer: completion style is a structural choice for your program, not a different I/O subsystem.

## Streaming completions

`lio` also has `IoStream<T>` for operations that conceptually yield more than one item over time.

That reinforces an important part of the design. `lio` distinguishes between:

- one completion for one registration
- repeated completions for one logical operation

Even if you do not use streaming operations directly, it helps to see that the crate has a separate shape for them. It shows that `lio` treats long-lived registrations as a first-class case rather than forcing every operation into a oneshot mold.

## What explanatory docs should say here

Rustdoc should list every method on `Io<T>`, `Receiver<T>`, `IoStream<T>`, and `StreamReceiver<T>`.

This chapter should answer different questions:

- Why are there multiple completion styles?
- Which style fits which program structure?
- What stays the same underneath them?

That is why it talks about tradeoffs and program shape instead of walking method by method through the API surface.

The next chapter goes one layer lower and explains the `OpModel` trait that makes these operation shapes composable inside the library.
