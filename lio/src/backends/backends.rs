//! I/O backend implementations for lio.
//!
//! This module provides the abstraction layer for different I/O backends and their
//! implementations. It defines the core trait [`IoBackend`] that all backends must
//! implement and manages the lifecycle of I/O operations.
//!
//! # Architecture
//!
//! The backend system is designed for thread-per-core runtime integration:
//!
//! - **[`IoBackend`]**: Unified trait for submission and completion. Each platform has
//!   specific implementations (io_uring on Linux, kqueue on macOS/BSD, IOCP on Windows).
//! - **[`OpStore`]**: Thread-local storage for in-flight operations.
//!
//! # Design Goals
//!
//! - **Zero runtime allocation**: All capacity is pre-allocated via [`init`](IoBackend::init)
//! - **Dyn-compatible**: Backends can be used as `Box<dyn IoBackend>`
//! - **Thread-local ownership**: Each thread owns its backend instance (`&mut self`)
//! - **Batched submission**: Push multiple ops, flush once
//!
//! # Example
//!
//! ```rust,ignore
//! use lio::backends::{IoBackend, OpStore};
//!
//! // Create and initialize backend
//! let mut backend = IoUring::default();
//! backend.init(1024)?;  // Pre-allocate for 1024 concurrent ops
//!
//! let mut store = OpStore::with_capacity(1024);
//!
//! // Submit operations
//! backend.push(id, &op)?;
//! backend.flush()?;
//!
//! // Poll for completions
//! let completions = backend.poll(&mut store)?;
//! ```

#[cfg(test)]
#[macro_use]
pub mod test_macro;

pub use impls::*;
mod impls {

  #[cfg(test)]
  pub mod dummy;

  #[cfg(linux)]
  pub mod io_uring;

  #[cfg(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "ios",
    target_os = "tvos",
    target_os = "watchos",
    target_os = "freebsd",
    target_os = "dragonfly",
    target_os = "openbsd",
    target_os = "netbsd"
  ))]
  pub mod pollingv2;

  #[cfg(windows)]
  mod iocp;
  #[cfg(windows)]
  pub use iocp::*;
}

mod store;
pub use store::*;

use std::io;
use std::time::Duration;

use crate::operation::Operation;

/// Error types that can occur when submitting operations to the backend.
#[derive(Debug)]
pub enum SubmitErr {
  /// An I/O error occurred during submission.
  Io(io::Error),
  /// The submission queue is full and cannot accept more operations.
  Full,
}

impl From<io::Error> for SubmitErr {
  fn from(value: io::Error) -> Self {
    Self::Io(value)
  }
}

/// Represents a completed I/O operation.
///
/// This type is returned by backend handlers when an operation completes,
/// containing the operation ID and its result.
#[derive(Debug)]
pub struct OpCompleted {
  /// The unique identifier of the completed operation.
  pub(crate) op_id: u64,

  /// Result of the operation:
  /// - `>= 0` on success (the return value, e.g., bytes transferred)
  /// - `< 0` on error (negative errno value)
  pub(crate) result: isize,
}

impl OpCompleted {
  /// Creates a new completed operation result.
  ///
  /// # Parameters
  ///
  /// - `op_id`: The unique ID of the operation
  /// - `result`: The operation result (non-negative for success, negative errno for error)
  pub fn new(op_id: u64, result: isize) -> Self {
    Self { op_id, result }
  }
}

/// Unified I/O backend trait for thread-per-core runtimes.
///
/// This trait combines submission and completion handling in a single interface,
/// designed to be owned by a single thread (`&mut self` everywhere). It's
/// dyn-compatible, allowing runtime selection of backends via `Box<dyn IoBackend>`.
///
/// # Lifecycle
///
/// 1. Create backend with `Default::default()` or backend-specific constructor
/// 2. Call [`init`](Self::init) to pre-allocate resources (zero runtime allocation)
/// 3. Use [`push`](Self::push) + [`flush`](Self::flush) to submit operations
/// 4. Use [`poll`](Self::poll) or [`wait`](Self::wait) to retrieve completions
///
/// # Thread Safety
///
/// Backends are designed for single-thread ownership. They are intentionally
/// not `Send` by default. Runtime authors control threading by creating one
/// backend per thread.
pub trait IoBackend {
  /// Initialize the backend with the given capacity.
  ///
  /// This pre-allocates all resources needed for `cap` concurrent operations.
  /// Must be called before any other methods. Calling init multiple times
  /// is undefined behavior.
  ///
  /// # Parameters
  ///
  /// - `cap`: Maximum number of concurrent in-flight operations
  ///
  /// # Errors
  ///
  /// Returns an error if resource allocation fails (e.g., io_uring setup fails).
  fn init(&mut self, cap: usize) -> io::Result<()>;

  /// Pushes an operation to the backend's submission queue without syscall.
  ///
  /// The operation is queued internally but not yet submitted to the kernel.
  /// Call [`flush`](Self::flush) to actually submit the queued operations.
  ///
  /// The caller guarantees that the operation is already registered in the [`OpStore`]
  /// with the given `id`.
  ///
  /// # Errors
  ///
  /// - [`SubmitErr::Full`]: Submission queue is full (call flush first)
  fn push(&mut self, id: u64, op: &dyn Operation) -> io::Result<()>;

  /// Flushes all queued operations to the kernel.
  ///
  /// This submits all operations queued via [`push`](Self::push) in a single syscall.
  /// After this call, the submission queue is empty and ready for new operations.
  ///
  /// # Returns
  ///
  /// The number of operations submitted.
  fn flush(&mut self) -> io::Result<usize>;

  /// Waits for completed operations with an optional timeout.
  ///
  /// - `timeout = None`: Block indefinitely until at least one operation completes
  /// - `timeout = Some(Duration::ZERO)`: Non-blocking poll, return immediately
  /// - `timeout = Some(duration)`: Wait up to `duration` for completions
  ///
  /// Returns an empty slice if the timeout expires with no completions.
  ///
  /// The returned slice is valid until the next call to `wait_timeout`, or `push`.
  fn wait_timeout(
    &mut self,
    store: &mut OpStore,
    timeout: Option<Duration>,
  ) -> io::Result<&[OpCompleted]>;
}
