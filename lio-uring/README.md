# lio-uring

A production-ready, safe, and ergonomic Rust interface to Linux's io_uring asynchronous I/O framework.

## Features

- **Safe API**: Minimal unsafe code with clear safety requirements
- **Zero overhead**: Thin wrapper around liburing with no runtime cost
- **Complete**: Support for all major io_uring operations
- **Production-ready**: Comprehensive error handling and documentation
- **Advanced features**:
  - Submission queue polling (SQPOLL)
  - IO polling (IOPOLL)
  - Linked operations
  - Fixed files and buffers
  - Batch submissions
  - Non-blocking completions

## Requirements

- Linux kernel 5.1+ (5.19+ recommended for full feature support)
- liburing is built from source during compilation

## Core Concepts

### Split API

The io_uring instance is split into two handles:

- **SubmissionQueue**: For submitting operations to the submission queue
- **CompletionQueue**: For retrieving completions from the completion queue

This design prevents accidental concurrent access and allows each half to be used from separate threads safely.

### Operations

All io_uring operations are defined in the `operation` module:

```rust
use lio_uring::operation::*;
use std::fs::File;
use std::os::fd::AsRawFd;

let (mut cq, mut sq) = lio_uring::with_capacity(128)?;

let file = File::create("/tmp/test")?;
let data = b"Hello, world!";

let write_op = Write {
    fd: file.as_raw_fd(),
    ptr: data.as_ptr() as *mut _,
    len: data.len() as u32,
    offset: 0,
};

unsafe { sq.push(&write_op, 1) }?;
```

### Safety

Operations contain raw pointers and file descriptors. When submitting, you must ensure:

1. All pointers remain valid until the operation completes
2. Buffers are not accessed mutably while operations are in flight
3. File descriptors remain valid until operations complete

### Error Handling

All operations return `io::Result` with proper error propagation:

```rust
let completion = cq.next()?;
match completion.result() {
    Ok(bytes) => println!("Transferred {} bytes", bytes),
    Err(e) => eprintln!("Operation failed: {}", e),
}
```

## Advanced Features

### Linked Operations

Chain operations to execute sequentially:

```rust
use lio_uring::SqeFlags;

// Write followed by fsync - fsync only runs if write succeeds
unsafe { sq.push_with_flags(&write_op, 1, SqeFlags::IO_LINK) }?;
unsafe { sq.push(&fsync_op, 2) }?;
sq.submit()?;
```

### Batch Submission

Submit multiple operations at once for better performance:

```rust
for (i, op) in operations.iter().enumerate() {
    unsafe { sq.push(op, i as u64) }?;
}
let submitted = sq.submit()?; // Submit all at once
```

### Non-blocking Completions

Check for completions without blocking:

```rust
while let Some(completion) = cq.try_next()? {
    println!("Got completion: {:?}", completion);
}
```

### Fixed Buffers and Files

Pre-register resources for zero-copy operations and improved performance.

#### Registered Buffers

Register buffers once, then reference them by index for zero-copy I/O:

```rust
use std::io::IoSlice;

// Prepare your buffers
let mut buffer1 = vec![0u8; 4096];
let mut buffer2 = vec![0u8; 4096];

// Register them with io_uring (pins them in kernel memory)
let buffers = vec![IoSlice::new(&buffer1), IoSlice::new(&buffer2)];
unsafe { sq.register_buffers(&buffers) }?;

// Use ReadFixed/WriteFixed with buf_index to reference registered buffers
let write_op = WriteFixed {
    fd: file.as_raw_fd(),
    buf: buffer1.as_ptr().cast(),  // Still provide the pointer
    nbytes: buffer1.len() as u32,
    offset: 0,
    buf_index: 0,  // Reference first registered buffer
};

unsafe { sq.push(&write_op, 1) }?;
sq.submit()?;

// When done, unregister
sq.unregister_buffers()?;
```

**Benefits:**
- Zero-copy I/O (data doesn't cross user/kernel boundary)
- Buffers are pinned in physical memory (no page faults)
- Reduced CPU overhead
- Significant performance improvement for frequently used buffers

#### Registered Files

Register file descriptors once, then reference by index:

```rust
// Register files
sq.register_files(&[fd1, fd2, fd3])?;

// Use operations with SqeFlags::FIXED_FILE and fd index
let read_op = Read {
    fd: 0,  // Index in registered files array
    ptr: buf.as_mut_ptr().cast(),
    len: buf.len() as u32,
    offset: 0,
};

unsafe { sq.push_with_flags(&read_op, 1, SqeFlags::FIXED_FILE) }?;

// Update specific indices
sq.register_files_update(1, &[new_fd])?;  // Replace fd at index 1

// Unregister when done
sq.unregister_files()?;
```

**Benefits:**
- Faster fd lookup (no fd table traversal)
- Reduced per-operation overhead

### IO Polling

Enable busy-polling for lowest latency:

```rust
use lio_uring::IoUringParams;

let params = IoUringParams::default().iopoll();
let (mut cq, mut sq) = IoUring::with_params(params)?;
```

## Supported Operations

### File I/O
- `Read`, `Write`, `ReadFixed`, `WriteFixed`
- `Readv`, `Writev` (vectored I/O)
- `Openat`, `Openat2`, `Close`, `CloseFixed`
- `Fsync`, `Fadvise`, `Fallocate`, `Ftruncate`
- `Statx`

### Network I/O
- `Socket`, `SocketDirect`
- `Accept`, `AcceptMulti` (multishot)
- `Connect`, `Bind`, `Listen`, `Shutdown`
- `Send`, `Recv`, `SendZc`, `RecvMulti` (multishot)
- `Sendmsg`, `Recvmsg`, `SendmsgZc`

### File System
- `Unlinkat`, `Renameat`, `Mkdirat`
- `Linkat`, `Symlinkat`
- `Getxattr`, `Setxattr`, `Fgetxattr`, `Fsetxattr`

### Advanced
- `Nop` (no operation, useful for testing)
- `Timeout`, `TimeoutRemove`, `LinkTimeout`
- `PollAdd`, `PollRemove`, `PollUpdate`
- `Cancel` (cancel pending operation)
- `Splice`, `Tee` (zero-copy data transfer)
- `SyncFileRange`
- `MsgRing` (cross-ring communication)
- `Futex` operations (futex wait/wake)
- `ProvideBuffers`, `RemoveBuffers` (buffer rings)

## Examples

See the `examples/` directory for complete working examples:

- `basic_io.rs` - Basic file I/O operations (open, read, write, fsync, unlink)
- `linked_ops.rs` - Linked operations and batch submission
- `tcp_echo.rs` - TCP echo server using sockets
- `registered_buffers.rs` - Zero-copy I/O with registered buffers (ReadFixed/WriteFixed)

Run an example:
```bash
cargo run --example basic_io
cargo run --example registered_buffers
```

## Performance Tips

1. **Batch submissions**: Submit multiple operations at once with a single `submit()` call
2. **Use fixed buffers/files**: Pre-register frequently used resources
3. **Enable IOPOLL**: For lowest latency on fast storage
4. **Enable SQPOLL**: Offload submission to a kernel thread
5. **Link dependent operations**: Reduce round-trips for sequential operations
6. **Size the ring appropriately**: Balance memory usage vs. batching opportunities

## Testing

Run the test suite:
```bash
cargo test
```

Note: Tests require a Linux system with io_uring support (kernel 5.1+).

## Contributing

Contributions are welcome! Please ensure:
- All tests pass
- New features include tests
- Public APIs are documented
- Code follows Rust style guidelines

## License

MIT License - see LICENSE file for details

## References

- [io_uring documentation](https://kernel.dk/io_uring.pdf)
- [liburing](https://github.com/axboe/liburing)
- [Linux kernel io_uring interface](https://www.kernel.org/doc/html/latest/io_uring.html)
