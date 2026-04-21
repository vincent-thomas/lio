# Getting Started

The fastest way to understand `lio` is to follow one operation all the way through.

The previous chapter described the control surface broadly. This chapter narrows the focus to one operation so the mechanics become concrete.

## A minimal example

```rust,no_run
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

## What each step means

### Create a driver

`Lio::new(64)` creates a driver with capacity for in-flight operations.

That value is not just configuration. It is the object that owns backend state, in-flight registrations, and timers.

### Build an operation

`api::sleep(...)` returns an `Io<T>`, not a completed result.

That value is a typed description of work that can be submitted.

### Bind the operation to a driver

Operations are not self-executing. `.with_lio(&lio)` associates the operation with the driver that should own its lifecycle.

You can avoid calling `.with_lio(...)` by installing a thread-local global `Lio` with `install_global(lio)`. That function now returns a guard, and dropping the guard uninstalls the global driver automatically.

That scoped installation model is convenient, but the explicit `.with_lio(&lio)` form is still the clearest way to learn the library.

For example:

```rust,no_run
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

```rust,no_run
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

## A real I/O example

The same model works for normal byte I/O:

```rust,no_run
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

## What to learn first

If you are new to `lio`, learn these ideas in order:

1. `Lio` is a driver, not a task runtime.
2. `Io<T>` is an operation handle, not a future by itself.
3. operations become live when you consume them
4. you must keep driving the loop until completions are processed
5. buffers and resources are part of the API contract

The next section makes that model explicit and then unpacks driving, ownership, and completion in more detail.
