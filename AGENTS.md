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

### Remaining platform-specific code in the main crate

No `libc`/`windows-sys` calls remain outside `backend/` (the backend
implementations plus `backend::native` and `backend::sockaddr`, the OS
primitives they share) and the `unstable_ffi` C interface. What is left is
inherent or confined:

- `Resource` implements the std raw descriptor/handle traits per platform
  (`AsRawFd`/`FromRawFd`, `AsRawHandle`/`FromRawHandle`).
- Lossless `OsString` <-> byte conversions use `std::os::unix::ffi` under
  `cfg(unix)`.
- Unit-test fixtures may create real descriptors with `libc`.

New code must keep it that way: OS primitives belong in `backend/`, and the
contract (`backend::op`) stays pure data.

## Testing contract

- When changing `lio::backend::op::Op` fields or semantic wrapper types, update the
  `lio-test` backend contract macro as well as `lio` unit/integration tests.
  `cargo test -p lio --lib` is not sufficient; run full `cargo test` to compile
  integration tests and doctests that instantiate raw `Op` values.
