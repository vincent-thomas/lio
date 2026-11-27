# lio-wasi

WASI 0.3 for WebAssembly components, without an executor.

`wasip3` exposes WASI 0.3 as Rust futures, and its streams read into an owned
`Vec`. Underneath, the component model works differently: `stream.read` takes
a pointer and a length, an async import returns a subtask, and a waitable set
reports which blocked operation finished. `lio-wasi` calls that layer
directly. No future is polled, and stream data is never copied inside the
component.

An operation is identified by what the component model itself waits on: the
stream it runs on, or the subtask of the call. It answers at once, or blocks
and ends in an event from a `WaitableSet`.

```rust
use lio_wasi::fs::{DescriptorFlags, OpenAt, OpenFlags, PathFlags};
use lio_wasi::{Event, Started, WaitableSet};

let mut set = WaitableSet::new();

let mut open = OpenAt::new(
  &dir, PathFlags::empty(), "data.txt", OpenFlags::empty(), DescriptorFlags::READ,
);
// SAFETY: `open` stays put until the call has returned.
if let Some(subtask) = unsafe { set.start(&mut open) } {
  assert_eq!(set.wait(), Some(Event::Returned(subtask)));
}
let file = open.result().unwrap()?;

let (stream, _status) = file.read_via_stream(0);
let stream = set.add_reader(stream);
let mut buf = [0u8; 4096];
// SAFETY: `buf` outlives the read, which has ended once this statement has.
let read = match unsafe { set.read(stream, buf.as_mut_ptr(), buf.len()) }? {
  Started::Done(read) => read,
  Started::Blocked => match set.wait() {
    Some(Event::Read(ended, read)) if ended == stream => read,
    other => unreachable!("only the read is in flight: {other:?}"),
  },
};
println!("{} bytes", read.count());
```

## Requirements

- Build for `wasm32-wasip3`. `WaitableSet::wait` blocks the calling task, and
  the component model lets only an async export do that. On `wasm32-wasip2`,
  `main` is a synchronous export and the first blocking wait traps.
- A host with WASI 0.3: `wasmtime run -S p3`.
- `alloc`. The crate is `no_std`, but WASI hands some results over as memory
  the host allocates in the component (names, datagrams, error messages), so
  an allocator has to be there. The `std` feature only adds the
  `core::error::Error` impls of the WASI error codes, which `wasip3` gates
  that way.

## Safety

`read`, `write`, `accept`, `read_dir`, `start` and `read_future` are `unsafe`.
If the operation blocks, the host uses the buffer or operation value after
the call returns, so the caller keeps it allocated, unmoved and untouched
until the operation's end is reported. Everything else is safe.

A blocked operation ends exactly once: in an `Event` from `wait` or `poll`,
or as the return value of its cancel or of closing its stream. A cancel
answers at once, and no event follows it.

## What is bound

Every function of `wasi:cli`, `wasi:clocks`, `wasi:filesystem`,
`wasi:sockets` and `wasi:random` is reachable from this crate, so a component
needs no other binding for them.

A function that waits is an operation here, listed by interface below. A
function that does not wait has nothing to start and await, so it is WASI's
own, passed on under the same name with nothing added:

- `cli::stdin`, `cli::stdout`, `cli::stderr`, `cli::environment`, `cli::exit`
  and the `cli::terminal_*` interfaces.
- `clock::now`, `clock::get_resolution` and `clock::system_clock`.
- `fs::preopens`, and the methods of `fs::Descriptor`.
- The methods of `sockets::TcpSocket` and `sockets::UdpSocket`.
- `random`.

`lio_wasi::stream` creates a byte stream, which is how WASI takes data to
write.

| WASI 0.3 | lio-wasi |
| --- | --- |
| `stream<u8>`, readable end: `descriptor.read-via-stream`, `tcp-socket.receive`, `stdin` | `WaitableSet::add_reader`, `read` |
| `stream<u8>`, writable end: `descriptor.write-via-stream`, `descriptor.append-via-stream`, `tcp-socket.send`, `stdout`, `stderr` | `WaitableSet::add_writer`, `write` |
| the `future<result>` that reports how such a stream ended | `fs::StreamStatus`, `sockets::StreamStatus`, `cli::StreamStatus`, read with `WaitableSet::read_future` |
| `descriptor.read-directory` (`stream<directory-entry>`) | `WaitableSet::add_dir_reader`, `read_dir` into an `fs::DirEntryBuf` |
| an async function, such as those below | its operation type, called with `WaitableSet::start` |
| `descriptor.open-at` | `fs::OpenAt` |
| `descriptor.stat`, `stat-at` | `fs::Stat`, `fs::StatAt` |
| `descriptor.get-flags`, `get-type` | `fs::GetFlags`, `fs::GetType` |
| `descriptor.is-same-object` | `fs::IsSameObject` |
| `descriptor.metadata-hash`, `metadata-hash-at` | `fs::MetadataHash`, `fs::MetadataHashAt` |
| `descriptor.sync`, `sync-data` | `fs::Sync`, `fs::SyncData` |
| `descriptor.set-size`, `advise` | `fs::SetSize`, `fs::Advise` |
| `descriptor.set-times`, `set-times-at` | `fs::SetTimes`, `fs::SetTimesAt` |
| `descriptor.create-directory-at`, `remove-directory-at`, `unlink-file-at` | `fs::CreateDirectoryAt`, `fs::RemoveDirectoryAt`, `fs::UnlinkFileAt` |
| `descriptor.rename-at`, `link-at`, `symlink-at`, `readlink-at` | `fs::RenameAt`, `fs::LinkAt`, `fs::SymlinkAt`, `fs::ReadlinkAt` |
| `tcp-socket.listen` (`stream<tcp-socket>`) | `WaitableSet::add_listener`, `accept` per connection |
| `tcp-socket.connect` | `sockets::Connect` |
| `udp-socket.send`, `receive` | `sockets::SendTo`, `sockets::ReceiveFrom` |
| `ip-name-lookup.resolve-addresses` | `sockets::ip_name_lookup::ResolveAddresses` |
| `monotonic-clock.wait-for`, `wait-until` | `clock::WaitFor`, `clock::WaitUntil` |
| `subtask.cancel` | `WaitableSet::cancel` |
| `future.cancel-read` | `WaitableSet::cancel_future` |
| `stream.cancel-read`, `stream.cancel-write` | `WaitableSet::cancel_read`, `cancel_write`, `cancel_accept`, `cancel_read_dir` |
| dropping a stream end | `WaitableSet::close_reader`, `close_writer`, `close_listener`, `close_dir_reader` |

Closing the stream given to `tcp-socket.send` shuts down the socket's write
side; closing its `receive` stream shuts down the read side.

### Worth knowing

- **One operation per stream end at a time.** Starting a second one
  on a busy stream returns `Error::Busy`.
- **A directory listing allocates.** The host allocates each entry's name in
  the component.
- **A UDP receive allocates.** WASI 0.3 hands a datagram over as a list the
  host allocates in the component; it cannot be received into caller memory.
  A name lookup's list of addresses is allocated the same way.
- **Files have no cursor.** `read_via_stream` and `write_via_stream` take an
  offset, like `pread` and `pwrite`.
- **Paths are UTF-8 strings.**

### Not bound

- `wasi:http`.

## Testing

Unit tests cover result decoding and layouts and run on any host:

```text
cargo test -p lio-wasi
```

The examples are the runtime tests. Each asserts what its operations did,
and does all of its I/O, its own output included, through this crate.
`mise run wasi` builds them for `wasm32-wasip3` and runs every one under
wasmtime.

## Maintenance

The argument and result layouts are written by hand to match what
`wit-bindgen` generates, and are pinned by compile-time size and offset
assertions. Import names carry the WASI version (`@0.3.2`), so moving to a
new WASI release means updating them together with the `wasip3` dependency.
