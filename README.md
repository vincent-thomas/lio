# lio

A low-level, fast, non-blocking I/O library using the most efficient mechanism available per platform.

[![Crates.io](https://img.shields.io/crates/v/lio.svg)](https://crates.io/crates/lio)
[![Documentation](https://docs.rs/lio/badge.svg)](https://docs.rs/lio)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](https://opensource.org/licenses/MIT)

## Platform Support

| Platform | Backend | Status |
|----------|---------|--------|
| Linux    | io_uring (epoll fallback) | Supported |
| macOS    | kqueue | Supported |
| Windows  | IOCP | Supported |

## Features

- **Zero-copy when possible**: Direct buffer access without intermediate copies
- **Callback-based API**: Non-blocking operations with completion callbacks
- **Platform-independent**: Single API across Linux, macOS, and Windows
- **Resource management**: Automatic lifecycle handling for file descriptors and handles
- **Optional buffer management**: Integrates with `bytes` crate and `zeroize` for secure memory

## Installation

Add to your `Cargo.toml`:

```toml
[dependencies]
lio = "0.3"
```

### Optional Features

```toml
[dependencies]
lio = { version = "0.3", features = ["bytes", "zeroize", "high"] }
```

| Feature | Description |
|---------|-------------|
| `bytes` | Integration with the `bytes` crate |
| `zeroize` | Secure memory zeroing on drop |
| `high` | Higher-level `net` and `fs` modules |
| `unstable_ffi` | C FFI bindings (unstable) |

## Quick Start

```rust
use lio::{Lio, api};

fn main() -> std::io::Result<()> {
    // Create an I/O driver with 64 entries
    let mut lio = Lio::new(64)?;

    // stdout (fd 1) is always valid
    let resource = unsafe { lio::api::resource::Resource::from_raw_fd(1) };
    let data = b"Hello, world!\n".to_vec();

    // Submit a write operation with a callback
    api::write(&resource, data).with_lio(&mut lio).when_done(|(result, buf)| {
        match result {
            Ok(bytes_written) => println!("Wrote {} bytes", bytes_written),
            Err(e) => eprintln!("Write failed: {}", e),
        }
        // buf is returned so you can reuse it
    });

    // Drive the I/O to completion
    lio.try_run()?;

    Ok(())
}
```

## Core Concepts

### Resources

Resources are platform-independent wrappers around OS handles (file descriptors on Unix, HANDLEs on Windows). They are reference-counted, so cloning is cheap and the underlying handle is automatically closed when the last reference is dropped.

```rust
use lio::api::resource::Resource;
use std::os::fd::FromRawFd;

// From an existing fd (unsafe - you must ensure validity)
let resource = unsafe { Resource::from_raw_fd(fd) };

// Clone shares the underlying fd
let resource2 = resource.clone();
```

### Operations

All I/O operations are non-blocking and return an `Io<T>` handle representing an in-flight operation:

```rust
use lio::api;

// File operations
api::openat(dir, path, flags, mode)
api::read(&resource, buffer)
api::write(&resource, data)
api::close(resource)
api::fsync(&resource)
api::truncate(&resource, length)

// Network operations
api::socket(domain, socket_type, protocol)
api::bind(&socket, address)
api::listen(&socket, backlog)
api::accept(&socket)
api::connect(&socket, address)
api::send(&socket, data, flags)
api::recv(&socket, buffer, flags)

// Other
api::timeout(duration)
api::nop()
```

### Driving I/O

Operations don't execute until you drive the `Lio` instance:

```rust
// Process all pending operations (non-blocking)
lio.try_run()?;

// Process operations with a timeout
lio.run_for(Duration::from_millis(100))?;
```

## Architecture

```
                    ┌─────────────────────────────────────┐
                    │            User Code                │
                    └─────────────────┬───────────────────┘
                                      │
                    ┌─────────────────▼───────────────────┐
                    │         lio (unified API)          │
                    └─────────────────┬───────────────────┘
                                      │
          ┌───────────────────────────┼───────────────────────────┐
          │                           │                           │
┌─────────▼─────────┐     ┌───────────▼───────────┐    ┌─────────▼─────────┐
│  io_uring/epoll   │     │        kqueue         │    │       IOCP        │
│     (Linux)       │     │     (macOS/BSD)       │    │     (Windows)     │
└───────────────────┘     └───────────────────────┘    └───────────────────┘
```

## Workspace Crates

| Crate | Description |
|-------|-------------|
| `lio` | Main library with platform abstraction |
| `lio-uring` | Safe Rust bindings to Linux io_uring |

## Testing

```bash
# Run all tests
cargo nextest run

# Run with specific backend
cargo nextest run --features "high"
```

## License

This project is licensed under the [MIT License](https://opensource.org/licenses/MIT).

## See Also

- [io_uring documentation](https://kernel.dk/io_uring.pdf)
- [kqueue man page](https://www.freebsd.org/cgi/man.cgi?query=kqueue)
- [IOCP documentation](https://docs.microsoft.com/en-us/windows/win32/fileio/i-o-completion-ports)
