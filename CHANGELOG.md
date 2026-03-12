# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Comprehensive README with usage examples and architecture documentation
- CHANGELOG.md for tracking changes

### Fixed

- **Resource lifecycle**: Fixed double-close bug where cloned `Resource` types would both try to close the same file descriptor on drop. Added `closed: AtomicBool` flag to track explicit closes.
- **Timeout handling**: Fixed synchronous timeout operations completing immediately with result: 0 instead of respecting the duration. Added timeout tracking (`timeouts: HashMap<u64, Instant>`) to the Poller.
- **Proptest write**: Fixed Resource lifecycle issue where temporary `Resource` was dropped immediately, causing EBADF errors on verification.
- **Shutdown tests**: Converted blocking `.recv()` calls to non-blocking loops with timeouts to prevent deadlocks in `test_shutdown_ipv6` and `test_shutdown_with_pending_data`.

### Changed

- **Operation representation**: Migrated from `Operation` trait to pure data `Op` enum for zero-copy operations
- **Buffer handling**: Added `BufPtr` type for passing buffer references to syscalls without taking ownership
- **Accept operation**: Changed to use `Arc<UnsafeCell<AcceptInner>>` for safe interior mutability

### Removed

- Unused macros (`impl_result`, `impl_no_readyness`) that were remnants of the old trait-based design
- Unused imports throughout the codebase (36+ warnings resolved)

## [0.3.0] - 2024

### Added

- Initial release with multi-platform I/O support
- Linux io_uring backend
- macOS kqueue backend
- Windows IOCP backend
- C FFI bindings (`unstable_ffi` feature)
- Comprehensive test suite including proptest-based fuzzing

[Unreleased]: https://github.com/vincent-thomas/lio/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/vincent-thomas/lio/releases/tag/v0.3.0
