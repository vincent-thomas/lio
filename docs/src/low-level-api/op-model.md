# OpModel

This chapter comes after driving, ownership, and completion style on purpose.

Most users can work productively with `lio` without thinking about `OpModel` immediately. It becomes useful once you want to understand how operations are implemented, how composed operations are built, or how to design new operation shapes as explicit state machines.

`OpModel` is the abstraction that turns raw completions into a logical operation.

It is the per-operation state machine that describes:

- what to do next
- how to interpret a completion
- whether the logical operation should continue, yield, or finish

This is one of the key design ideas in `lio`, but it is easier to understand once the user-facing model is already clear. At the surface, users see typed operations. Underneath, `OpModel` is the reason those operations can represent one step, many steps, or a stream without changing the overall programming model.

## Core contract

The trait in `lio/src/api/op.rs` is intentionally small.

```rust
pub trait OpModel: Send + 'static {
    type Item: Send;

    fn action(&mut self) -> Action;
    fn complete(&mut self, completion: Completion) -> OpResult<Self::Item>;
}
```

That small surface is deliberate. `OpModel` is not trying to be a full framework trait. It only answers two questions:

- what is the next step?
- what does this completion mean?

Everything else in the operation lifecycle is built on top of those two methods. That narrow interface is what lets the trait stay composable instead of becoming a mini-runtime API of its own.

## The return types matter

`action()` does not return a user value. It returns an `Action`, which is a description of the next step the model wants.

Today, that means one of:

- `Action::Io(Op)` for a backend I/O step
- `Action::Sleep(Duration)` for a timer step

`complete()` returns an `OpResult<Self::Item>`, which expresses the model's interpretation of one completion:

- `Again`: the logical operation is still in progress and has more work to describe
- `Yield(item)`: the model produced one item but remains live
- `Done(item)`: the model reached its terminal state

That is the entire contract. A model does not need any additional hooks to express one-shot, multi-step, or streaming behavior. The rest of the chapter is mostly different consequences of that small interface.

## Two kinds of models

The marker traits split models into two broad categories.

### `OneshotOpModel`

These produce exactly one terminal value.

These are consumed through `Io<T>`.

### `StreamOpModel`

These may produce multiple items over time.

The clearest current example is `ops::Interval`, which yields on every timer firing.

These are consumed through `IoStream<T>`.

The distinction is useful because it separates two user-visible operation shapes without widening the core contract itself.

## The simplest possible model

`Sleep` is a good minimal example.

Its `action()` always returns:

```rust,ignore
Action::Sleep(self.duration)
```

and its `complete()` interprets the timer completion and immediately returns:

```rust,ignore
OpResult::Done(result)
```

That is a one-step state machine with no internal transition beyond "not done" to "done".

That makes `Sleep` a useful baseline when you are writing your own model. If your first draft feels much more complicated than this kind of single-step operation requires, you are probably mixing policy and mechanism too early.

## A streaming model

`Interval` is almost identical to `Sleep`, except for one critical difference:

```rust,ignore
OpResult::Yield(result)
```

instead of `Done`.

That means the model remains live after each timer completion. Since `action()` still returns the same `Action::Sleep(period)`, the model becomes a repeating timer.

This is an important design point:

- a stream model does not need a different trait
- it only needs to return `Yield` instead of `Done`

That is one of the stronger properties of `OpModel`: one contract is enough for both "exactly once" and "many times over time."

## A sequential multi-step state machine

`net::ops::TcpBindListener` is the clearest real example of a composed logical operation.

Binding a TCP listener is not one kernel action. It is a sequence:

1. create a socket
2. bind it
3. listen on it

The model encodes that with an internal enum:

```rust,ignore
enum TcpBindState {
    Socket(ops::Socket),
    Bind { resource: Resource },
    Listen { resource: Resource },
    Done,
}
```

Then:

- `action()` matches on the current state and returns the corresponding next step
- `complete()` interprets the completion for that state and mutates the state machine forward

The transitions are:

- `Socket` success -> `Bind`
- `Bind` success -> `Listen`
- `Listen` success -> `Done(Ok(TcpListener))`
- any failure -> `Done(Err(...))`

This is the pattern to copy when one logical operation requires several low-level actions.

The lesson is not the exact TCP sequence. It is that state names should reflect logical steps. Good state names make `action()` and `complete()` read like a transition table instead of a pile of conditionals.

## Composition patterns in this repository

The codebase already uses several distinct kinds of composition. Seeing them side by side helps because they solve different problems and should not be collapsed into one pattern.

### 1. Wrapper composition

Some models simply wrap another model and adapt its output type.

Examples:

- `io::Copy` wraps `CopyN`
- `net::ops::SocketAccept` wraps `ops::Accept`
- `net::ops::TcpAccept` wraps `ops::Accept`

This is the lightest form of composition. The wrapper delegates the state machine to another model and only changes the user-facing result.

Use this when:

- the underlying step structure is already correct
- you only need to map result types
- you want a more specialized public abstraction

### 2. Sequential state composition

Some models compose several low-level operations into one higher-level logical unit.

Examples:

- `TcpBindListener`
- `TcpConnectSocket`
- `CopyN`

These models own explicit state and move from one step to the next as completions arrive.

Use this when:

- one user operation requires multiple backend submissions
- later steps depend on results from earlier steps
- you need to carry owned state across steps

### 3. Streaming composition

Some models are designed to stay alive across many completions.

Examples:

- `Interval`
- any future multishot or repeating operation

Use this when:

- the logical operation is open-ended or long-lived
- one registration should produce several values

## Designing an OpModel as a state machine

The best way to design an `OpModel` in this codebase is to start by writing the states down explicitly.

Do not start with code. Start with a transition table. If the transitions are vague on paper, they will be vague in `complete()` too.

For a oneshot model, ask:

- what is the first action?
- what completion shapes can arrive?
- on success, what is the next state?
- on error, do we terminate immediately?
- what value do we return at the end?

For a stream model, add:

- when do we yield?
- what keeps the model alive after yielding?
- what condition ends the stream?

## A practical recipe

### 1. Define the logical steps

If the operation is more than one backend action, make the steps explicit.

For example:

```rust,ignore
enum State {
    Open,
    ReadHeader { fd: Resource, buf: Vec<u8> },
    ReadBody { fd: Resource, buf: Vec<u8>, expected: usize },
    Done,
}
```

This is better than sprinkling booleans through the struct. Explicit states make invalid transitions harder to express.

### 2. Put the carried data in the states or the model

The model must own whatever survives across completions:

- resources
- buffers
- counters
- offsets
- addresses
- parsed intermediate values

The rule is simple: if the next step needs it, the model must own it somewhere.

### 3. Make `action()` a pure projection of state

In a well-structured model, `action()` should mostly be "given the current state, what is the next runtime action?"

That keeps it easy to reason about and easy to test.

### 4. Make `complete()` perform the transition

`complete()` should:

- inspect the current state
- interpret the completion for that state
- move to the next state
- return `Again`, `Yield`, or `Done`

If `action()` starts mutating state heavily, the model is usually getting harder to understand than necessary.

### 5. Reserve panic for impossible usage

The current codebase commonly uses panics for programmer errors such as:

- polling after `Done`
- receiving impossible `Yield` from a wrapped oneshot inner model

That is a reasonable pattern here. Kernel or I/O failures should become `Err(...)` results. Broken internal protocol assumptions should fail loudly.

## Designing for composition

If you want a model that composes well with other code, keep the boundaries clean.

### Prefer wrapping over rewriting

If an existing model already owns the tricky low-level semantics, wrap it and map the result rather than duplicating its logic.

The socket wrappers in `lio/src/net/ops.rs` are good examples of this.

### Keep step boundaries explicit

If a model does several things, represent those as states instead of blending them into one giant `complete()` branch.

### Return ownership deliberately

If buffers or resources need to survive across steps, ensure the model returns or reuses them intentionally. `CopyN` is a good example: it carries one buffer through alternating read/write phases and resubmits partial writes correctly.

## `CopyN` as a richer example

`CopyN` is a good model to study because it shows several design techniques at once.

Its states are:

- `Reading(ops::Read<Vec<u8>>)`
- `Writing(ops::Write<Vec<u8>>)`
- `Done`

Its transitions are:

- read returns EOF -> `Done(Ok(total))`
- read returns bytes -> truncate buffer to actual length, move to `Writing`
- write returns partial progress -> keep remaining bytes, stay in `Writing`
- write completes full buffer -> resize buffer and go back to `Reading`

That is a real state machine, not just a thin wrapper around a syscall.

It also shows a recurring design pattern in `lio`:

- reuse inner `ops::*` models when they already encode a low-level contract correctly
- build higher-level sequencing around them

## Testing the contract

The repository includes `OpModelContract` support in the `lio-test` crate, and `lio` uses that support to script contract tests for concrete models.

That contract machinery tests the logical protocol:

- does `action()` request the expected action?
- does `complete()` return the expected `OpResult`?
- does a oneshot terminate correctly?
- does a stream yield or remain live correctly?

This is the right kind of test for `OpModel`.

It does not try to test the kernel. It tests the state machine.

For model-level tests, prefer checking:

- the sequence of actions
- the sequence of state transitions
- final output values
- correct handling of error paths

This is a good fit for `OpModel` because the abstraction is already a state machine contract. The most important tests are usually not "did the kernel work?" but "did this model interpret each step correctly?"

That is also why contract tests are worth separating from ordinary integration tests. Integration tests tell you whether a backend and the operating system produced the expected behavior. Contract tests tell you whether the model itself is well-formed.

## A minimal skeleton

This is the right shape for many composed models:

```rust,ignore
use std::io;
use lio::api::op::{Action, Completion, OpModel, OpResult};

struct MyModel {
    state: State,
}

enum State {
    Step1,
    Step2 { value: usize },
    Done,
}

impl OpModel for MyModel {
    type Item = io::Result<usize>;

    fn action(&mut self) -> Action {
        match self.state {
            State::Step1 => { /* return first action */ }
            State::Step2 { value } => { /* return second action using value */ }
            State::Done => panic!("MyModel polled after completion"),
        }
    }

    fn complete(&mut self, completion: Completion) -> OpResult<Self::Item> {
        match self.state {
            State::Step1 => {
                if completion.result < 0 {
                    self.state = State::Done;
                    OpResult::Done(Err(io::Error::from_raw_os_error(
                        (-completion.result) as i32,
                    )))
                } else {
                    let value = completion.result as usize;
                    self.state = State::Step2 { value };
                    OpResult::Again
                }
            }
            State::Step2 { .. } => {
                self.state = State::Done;
                if completion.result < 0 {
                    OpResult::Done(Err(io::Error::from_raw_os_error(
                        (-completion.result) as i32,
                    )))
                } else {
                    OpResult::Done(Ok(completion.result as usize))
                }
            }
            State::Done => panic!("MyModel completed after terminal state"),
        }
    }
}
```

The exact details will vary, but the structure should look familiar after reading the real models in the repository.

## The main idea

`OpModel` is where `lio` turns "one or more backend actions" into "one logical operation".

That is why it is so central.

It gives the library:

- typed results
- explicit step sequencing
- reusable wrappers
- streaming support
- testable state-machine behavior

And it gives you, as an implementor, a clear design rule:

build the operation as a state machine, let `action()` describe the next step, and let `complete()` own the transition.
