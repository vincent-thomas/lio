# Project notes for lio

## Design philosophy

`lio` is **pure bookkeeping**. Its job is to track operations, manage buffers, route
completions, and provide type-safe abstractions — nothing more.

- **All actual I/O** (syscalls, kernel submissions, polling) belongs in `IoBackend`
  implementations (`backend/impls/`). The `IoBackend` trait and `Op` data types in
  `backend/op.rs` define the contract surface; they hold data, not platform code.
- **No `libc`**, no `windows-sys`, no `cfg(target_os)` conditional code shall appear
  outside of a backend implementation crate. The main `lio` crate must be
  platform-independent.
- **Allocation** (future goal) should follow the same pattern — outsourced to a
  pluggable allocator interface.

### Current OS-dependent leakage (to be eliminated)

These locations in the main `lio` crate contain platform-specific code that should
be pushed into backend implementations or abstracted behind the contract:

| Location | What depends on OS |
|---|---|
| `lio/src/macros.rs` | `syscall!` macro wraps `libc` directly |
| `lio/src/lio.rs` | `SLEEP_RESULT` constant uses `libc::ETIME` / `libc::ETIMEDOUT` |
| `lio/src/api/ops.rs` | Socket addr conversion uses `libc::sockaddr_*`; `MSG_DONTWAIT`, `MSG_NOSIGNAL`, `S_IFREG`, `ERANGE` constants; `libc::dup` / `libc::STDIN_FILENO` for pipe/tee models |
| `lio/src/api/resource.rs` | `libc::STDIN_FILENO`, `STDOUT_FILENO`, `STDERR_FILENO`, `AT_FDCWD` |
| `lio/src/fs.rs` | `libc::O_*` flags, `libc::AT_REMOVEDIR`, `libc::EINVAL` |
| `lio/src/net/socket.rs` | `libc::sockaddr_storage`, `libc::getsockname`, `libc::getpeername` |
| `lio/src/net/unix.rs` | `libc::AF_UNIX`, `libc::SOCK_STREAM`, `libc::fcntl`, `libc::bind`, `libc::listen`, `libc::connect`, `libc::sockaddr_un` |
| `lio/src/platform/errno.rs` | `cfg(target_os)` for `__errno_location` / `__error` / `GetLastError` |
| `lio/src/api/mod.rs` | Imports and re-exports `SockDomain`, `SockProto`, etc. from `backend::op` |

Each of these must either:
1. Be moved into a backend implementation, or
2. Be lifted into a platform-independent abstraction in `backend::op` (pure data types)
   with the OS-specific translation happening inside the backend.

## Testing contract

- When changing `lio::backend::op::Op` fields or semantic wrapper types, update the
  `lio-test` backend contract macro as well as `lio` unit/integration tests.
  `cargo test -p lio --lib` is not sufficient; run full `cargo test` to compile
  integration tests and doctests that instantiate raw `Op` values.
