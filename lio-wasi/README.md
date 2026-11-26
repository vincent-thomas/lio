# lio-wasi

A completion queue over WASI 0.3, for WebAssembly components. It is to WASI
what `lio-uring` is to io_uring: push an operation with a `user_data`, wait,
and get a completion carrying that `user_data` back.

`wasip3` exposes WASI 0.3 as Rust futures, and its streams read into an owned
`Vec`. Underneath, the component model is already a completion queue:
`stream.read` takes a pointer and a length, and a waitable set reports which
blocked operation finished. `lio-wasi` calls that layer directly. No future
is polled, and stream data is never copied inside the component.

```rust
use lio_wasi::fs::{DescriptorFlags, OpenAt, OpenFlags, PathFlags};
use lio_wasi::{Outcome, Queue};

let mut queue = Queue::new();

let mut open = OpenAt::new(
  &dir, PathFlags::empty(), "data.txt", OpenFlags::empty(), DescriptorFlags::READ,
);
// SAFETY: `open` stays put until its completion is taken below.
unsafe { queue.push_call(&mut open, 1) };
assert_eq!(queue.wait().unwrap().outcome(), Outcome::Returned);
let file = open.result().unwrap()?;

let (stream, _status) = file.read_via_stream(0);
let stream = queue.add_reader(stream);
let mut buf = [0u8; 4096];
// SAFETY: `buf` outlives the completion taken below.
unsafe { queue.push_read(stream, buf.as_mut_ptr(), buf.len(), 2) }?;
let read = queue.wait().unwrap().outcome().count();
```

## Requirements

- Build for `wasm32-wasip3`. `Queue::wait` blocks the calling task, and the
  component model lets only an async export do that. On `wasm32-wasip2`,
  `main` is a synchronous export and the first blocking wait traps.
- A host with WASI 0.3: `wasmtime run -S p3`.

## Safety

`push_read`, `push_write`, `push_accept`, `push_read_dir` and `push_call` are
`unsafe`. The
host uses the buffer or operation value after the call returns, so the caller
keeps it allocated, unmoved and untouched until its completion has been
returned. Everything else is safe, and every pushed operation produces
exactly one completion, cancelled or not.

## Operations

How each `lio-uring` operation maps onto WASI 0.3.

| io_uring | lio-wasi |
| --- | --- |
| `Nop` | `Nop` |
| `Read`, `Readv`, `ReadFixed` | `Queue::push_read` on a `read_via_stream` or `TcpSocket::receive` stream. One buffer per read. |
| `Write`, `Writev`, `WriteFixed` | `Queue::push_write` on a stream given to `write_via_stream`, `append_via_stream` or `TcpSocket::send` |
| read and write errors | `fs::StreamStatus`, `sockets::StreamStatus` |
| `OpenAt` | `fs::OpenAt` |
| `Close` | drop the `Descriptor` or socket; `Queue::close_*` for a stream |
| `Statx` | `fs::Stat`, `fs::StatAt` |
| `Fsync` | `fs::Sync`, `fs::SyncData` |
| `Fallocate` | `fs::SetSize` (changes the size; does not reserve blocks) |
| `Fadvise` | `fs::Advise` |
| `RenameAt` | `fs::RenameAt` |
| `UnlinkAt` | `fs::UnlinkFileAt`, `fs::RemoveDirectoryAt` |
| `MkDirAt` | `fs::CreateDirectoryAt` |
| `SymlinkAt`, `LinkAt` | `fs::SymlinkAt`, `fs::LinkAt` |
| `Socket`, `Bind`, `Listen` | `wasip3`: `create`, `bind`, `listen` do not wait |
| `Accept`, `AcceptMulti` | `Queue::add_listener`, then `Queue::push_accept` per connection |
| `Connect` | `sockets::Connect`; UDP `connect` does not wait |
| `Send`, `Recv` (TCP) | `Queue::push_write`, `Queue::push_read` |
| `SendMsg`, `RecvMsg` (UDP) | `sockets::SendTo`, `sockets::ReceiveFrom` |
| `Shutdown` | `Queue::close_writer` for the write side, `Queue::close_reader` for the read side |
| `Timeout`, `LinkTimeout` | `clock::WaitFor`, `clock::WaitUntil`, pushed next to the operation they bound |
| `TimeoutRemove`, `AsyncCancel` | `Queue::cancel` |

Beyond io_uring's set: `fs::ReadlinkAt`, `fs::SetTimes`, `fs::SetTimesAt`,
`fs::GetFlags`, `fs::GetType`, and directory listing through
`Queue::add_dir_reader` and `Queue::push_read_dir` into an `fs::DirEntryBuf`.

### No WASI 0.3 equivalent

- `PollAdd`, `PollRemove`, `EpollCtl`: there is no readiness interface.
- `Splice`, `Tee`, `SyncFileRange`, `Madvise`.
- Extended attributes (`GetXattr`, `SetXattr` and the `F` forms).
- Registered files and buffers, provided buffers, multishot receive,
  SQPOLL, linked submissions.
- `SetSockOpt`: socket options are plain `wasip3` calls that do not wait.

### Differences worth knowing

- **One operation per stream end at a time.** A second push on a busy
  stream returns `Error::Busy`.
- **A directory listing allocates.** The host allocates each entry's name in
  the component.
- **A UDP receive allocates.** WASI 0.3 hands a datagram over as a list the
  host allocates in the component; it cannot be received into caller memory.
- **Files have no cursor.** `read_via_stream` and `write_via_stream` take an
  offset, like `pread` and `pwrite`.
- **Paths are UTF-8 strings.**

### Not bound yet

- `ip-name-lookup.resolve-addresses`.
- `descriptor.is-same-object`, `metadata-hash`, `metadata-hash-at`.

## Testing

Unit tests cover result decoding and layouts and run on any host:

```text
cargo test -p lio-wasi
```

The examples are the runtime tests. Each asserts what its operations did.
`mise run wasi` builds them for `wasm32-wasip3` and runs every one under
wasmtime.

## Maintenance

The argument and result layouts are written by hand to match what
`wit-bindgen` generates, and are pinned by compile-time size and offset
assertions. Import names carry the WASI version (`@0.3.2`), so moving to a
new WASI release means updating them together with the `wasip3` dependency.
