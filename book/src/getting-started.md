# Getting Started

The fastest way to understand `lio` is to follow one operation all the way through.

The previous chapter named the parts. This chapter shows how those parts fit together in actual code.

If you read nothing else before trying the crate, read this chapter. It contains the smallest complete example of the user-facing model.

## A minimal example

```rust,no_run,edition2024
# extern crate lio;
use lio::{Lio, api};
use std::time::Duration;

fn main() -> std::io::Result<()> {
    let lio = Lio::new(64)?;

    let mut rx = api::sleep(Duration::from_millis(100))
        .with_lio(&lio)
        .send();

    loop {
        if let Some(result) = rx.try_recv() {
            result?;
            break;
        }

        if lio.try_run()? == 0 {
            lio.run()?;
        }
    }

    Ok(())
}
```

This example contains the whole user-level model:

1. Create a driver.
2. Build an operation.
3. Bind it to that driver with `.with_lio(&lio)`.
4. Choose a completion strategy.
5. Drive the event loop until the result arrives.

If that list already makes intuitive sense, you have the right high-level mental model. The rest of the book mainly refines each step.

Another way to say the same thing is:

- `Lio` owns progress
- the operation value describes the work
- consuming the operation starts it
- the completion strategy decides how the result comes back

## What each step means

### Create a driver

`Lio::new(64)` creates a driver with capacity for in-flight operations.

That value is not just configuration. It is the object that owns backend state, in-flight registrations, and timers. In `lio`, the driver is a first-class value because progress is a first-class responsibility.

If you are used to runtimes where the driver is hidden, this is the first conceptual shift: in `lio`, the event source is a value your code owns directly.

### Build an operation

`api::sleep(...)` returns an `Io<T>`, not a completed result.

That value is a typed description of work that can be submitted.

That is why `lio` feels explicit even before anything runs. You can hold the operation, pass it around, bind it to a driver, and choose how it should complete.

That explicitness is one of the crate's recurring themes. `lio` prefers a visible operation value over a design where work starts implicitly as soon as you mention it.

### Bind the operation to a driver

Operations are not self-executing. `.with_lio(&lio)` associates the operation with the driver that should own its lifecycle.

You can avoid calling `.with_lio(...)` by installing a thread-local global `Lio` with `install_global(lio)`. That function now returns a guard, and dropping the guard uninstalls the global driver automatically.

That scoped installation model is convenient, but the explicit `.with_lio(&lio)` form is still the clearest way to learn the library.

For example:

```rust,no_run,edition2024
# extern crate lio;
use lio::{Lio, api, install_global};
use std::time::Duration;

fn main() -> std::io::Result<()> {
    let lio = Lio::new(64)?;
    let _guard = install_global(lio.clone());

    let mut rx = api::sleep(Duration::from_millis(10)).send();

    loop {
        if let Some(result) = rx.try_recv() {
            result?;
            break;
        }

        if lio.try_run()? == 0 {
            lio.run()?;
        }
    }

    Ok(())
}
```

The guard is what keeps the installation alive. If it is dropped immediately, the global installation ends immediately too.

### Choose a completion strategy

Here we used `.send()` to get a receiver.

That is convenient for a first example because it makes completion state explicit. In other situations, `.await` or `.when_done(...)` may be a better fit.

### Drive the loop

Many async libraries hide this part. `lio` does not.

The pattern:

```rust,no_run,edition2024
# extern crate lio;
# use lio::{Lio, api};
# fn run_one<T>(lio: &Lio, mut rx: api::Receiver<T>) -> T {
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

appears throughout the repository, including examples and the BusyBox-style sample application.

If one part of `lio` feels unfamiliar at first, it is usually this one. The crate does not assume somebody else is responsible for forward progress. Your code is.

Once that idea feels natural, the rest of the crate gets easier. Progress, ownership, and completion all become easier to reason about when there is no hidden scheduler in the background.

## A real I/O example

The same model works for normal byte I/O:

```rust,no_run,edition2024
# extern crate lio;
use lio::{Lio, api};
use lio::api::resource::Resource;

fn main() -> std::io::Result<()> {
    let lio = Lio::new(64)?;
    let stdout = Resource::stdout();

    let mut rx = api::write(&stdout, b"hello\n".to_vec())
        .with_lio(&lio)
        .send();

    loop {
        if let Some((result, _buf)) = rx.try_recv() {
            result?;
            break;
        }

        if lio.try_run()? == 0 {
            lio.run()?;
        }
    }

    Ok(())
}
```

Notice that the buffer comes back out of the operation. That is expected and intentional.

That detail is worth noticing early because it is not a special case for `write`. It is part of the general ownership model of the crate.

## What to learn first

If you are new to `lio`, learn these ideas in order:

1. `Lio` is a driver, not a task runtime.
2. `Io<T>` is an operation handle, not a future by itself.
3. operations become live when you consume them
4. you must keep driving the loop until completions are processed
5. buffers and resources are part of the API contract

The next section turns those observations into a more general model. It starts with the low-level API because that is where the design is most visible.
