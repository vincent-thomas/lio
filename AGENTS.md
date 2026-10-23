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

## Panics

- No `debug_assert!`: CI rejects it, since it is compiled out of release
  builds. Use `assert!`.
- Library code denies `clippy::unwrap_used` and `clippy::expect_used`
  (`clippy.toml` exempts test code), so every panic is spelled as one and
  reads as intentional. Return an error or restructure where you can. Where a
  panic is deliberate, write it out with `let ... else`:
  `unreachable!` for a broken internal invariant, `panic!` for caller misuse
  (document it under `# Panics`). Don't silence the lints with `#[allow]` or
  `#[expect]`.

## Testing contract

- When changing `lio::backend::op::Op` fields or semantic wrapper types, update the
  `lio-test` backend contract macro as well as `lio` unit/integration tests.
  `cargo test -p lio --lib` is not sufficient; run full `cargo test` to compile
  integration tests and doctests that instantiate raw `Op` values.
- Core bookkeeping is tested without I/O through `lio::backend::testing`
  (`ScriptedBackend`, `Checked`). Tests that need a real backend are marked
  `#[cfg_attr(miri, ignore = "needs a real backend")]`; `scripts/test-miri.sh`
  runs the rest under Miri.
- Search sizes: `PROPTEST_CASES` (model tests) and `LIO_LIFECYCLE_DEPTH`
  (lifecycle enumeration). CI runs them small on PRs and large nightly
  (`.github/workflows/deep.yaml`), along with every cargo-mutants mutant of
  the files `.cargo/mutants.toml` lists. A surviving mutant in a PR's diff
  fails CI: add a test that kills it.
