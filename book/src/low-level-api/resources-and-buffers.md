# Resources and Buffers

If you want `lio` code to feel straightforward, learn the ownership model early.

Once you already understand that `Lio` must be driven explicitly, this is the next thing to internalize. Most of the surface-level strangeness in `lio` is really ownership made visible.

## Resources are shared ownership of OS objects

`api::resource::Resource` is the library’s wrapper around a file descriptor or platform handle.

The practical consequence is:

- the underlying OS object has shared ownership
- cloning is cheap
- cleanup happens when the last owner goes away

That is why so much of the API takes `&impl AsResource`. An operation needs access to a file, socket, or directory handle, but the caller often wants to keep using the wrapper after submission.

## Buffers move into operations

For byte-oriented operations such as `read`, `write`, `recv`, and `send`, the buffer is moved into the operation and returned on completion.

That pattern can look unusual if you expect borrow-based async I/O. In `lio`, buffer ownership is part of the contract rather than an implementation detail hidden behind a future.

It means:

- the kernel never outlives the buffer
- the operation can hold ownership for as long as needed
- the caller gets the same allocation back for reuse

A typical read looks like this:

```rust,no_run
extern crate lio;
use lio::{Lio, api};
use lio::api::resource::Resource;

fn main() -> std::io::Result<()> {
    let lio = Lio::new(64)?;
    let stdin = Resource::stdin();
    let mut rx = api::read(&stdin, vec![0u8; 4096]).with_lio(&lio).send();

    loop {
        if let Some((result, buf)) = rx.try_recv() {
            let n = result? as usize;
            let _bytes = &buf[..n];
            break;
        }

        if lio.try_run()? == 0 {
            lio.run()?;
        }
    }

    Ok(())
}
```

The return type tells you two things at once:

- whether the operation succeeded
- which buffer is safe to use again

That second point is the one to internalize. Completion does not only report status. It also hands ownership back at the exact point where reuse becomes safe.

## Why this matters for filesystem and networking code

Because the operation layer is close to the syscall layer, `lio` often asks you to be explicit about:

- which directory a path is relative to
- which socket or file a transfer targets
- which exact buffer is participating in an operation

That explicitness is not noise around the real API. It is part of how the crate keeps ownership and lifecycle rules visible in user code instead of burying them in runtime internals.

## The core ownership pattern

The most important pattern to understand is:

- resources are reference-counted wrappers around OS objects
- buffers are owned by operations while in flight
- results often return both status and ownership

Once those ideas are clear, techniques like channel reuse become ordinary coordination patterns rather than mysterious magic.

After ownership is clear, the next question is how those completed operations should be delivered back into your program. That is where the completion models fit.
