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
//! ```
//! use lio::backend::{IoBackend, op::Op, pollingv2::Poller};
//! use std::time::Duration;
//!
//! // Create and initialize backend
//! let mut backend = Poller::default();
//! backend.init(1024).unwrap();  // Pre-allocate for 1024 concurrent ops
//!
//! // Submit a nop operation
//! backend.push(1, Op::Nop).unwrap();
//! backend.flush().unwrap();
//!
//! // Poll for completions (non-blocking)
//! let completions = backend.wait_timeout(Some(Duration::ZERO)).unwrap();
//! ```

pub mod op;

/// Test utilities for IoBackend implementations.
///
/// Use the [`test_io_backend!`] macro to generate a comprehensive test suite
/// for your IoBackend implementation.
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

pub(crate) mod store;

use std::io;
use std::time::Duration;

use crate::backend::op::Op;

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

  /// True if this is a multishot operation and more completions are coming.
  ///
  /// For io_uring, this is set based on the `IORING_CQE_F_MORE` flag.
  /// For other backends using resubmission, this is always true until
  /// the stream is explicitly terminated.
  pub(crate) more: bool,
}

impl OpCompleted {
  /// Creates a new completed operation result.
  ///
  /// # Parameters
  ///
  /// - `op_id`: The unique ID of the operation
  /// - `result`: The operation result (non-negative for success, negative errno for error)
  pub fn new(op_id: u64, result: isize) -> Self {
    Self { op_id, result, more: false }
  }

  /// Sets the `more` flag indicating more completions are coming.
  pub fn with_more(mut self, more: bool) -> Self {
    self.more = more;
    self
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
/// 4. Use [`wait_timeout`](Self::wait_timeout) to retrieve completions
///
/// # Timer Support
///
/// Backends provide a single kernel timer via [`arm_timer`](Self::arm_timer).
/// This is used by the timing wheel to multiplex many logical timers onto
/// one kernel resource. When the timer fires, `wait_timeout` returns, and
/// the caller should poll the timing wheel for expired timers.
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
  fn push(&mut self, id: u64, op: Op) -> io::Result<()>;

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
    timeout: Option<Duration>,
  ) -> io::Result<&[OpCompleted]>;

  /// Arms the backend's single kernel timer to fire after `duration`.
  ///
  /// This is used by the timing wheel to wake `wait_timeout` when the
  /// earliest timer expires. Only one timer can be armed at a time;
  /// calling this again replaces the previous timer.
  ///
  /// When the timer fires, `wait_timeout` will return (possibly with
  /// an empty completion slice if no I/O completed).
  fn arm_timer(&mut self, duration: Duration) -> io::Result<()>;

  /// Disarms the kernel timer if one is armed.
  ///
  /// This is a no-op if no timer is currently armed.
  fn disarm_timer(&mut self) -> io::Result<()>;

  /// Pushes a streaming operation that yields multiple completions.
  ///
  /// Unlike regular `push`, this method is for operations that produce
  /// multiple results over time (e.g., accept loops, file watches).
  ///
  /// Backend behavior:
  /// - **io_uring**: Uses native multishot operations (e.g., `AcceptMulti`)
  ///   where a single submission yields multiple completions with `IORING_CQE_F_MORE`.
  /// - **pollingv2**: Stores the operation and automatically resubmits after
  ///   each completion to simulate multishot behavior.
  ///
  /// The default implementation falls back to regular `push`, which works
  /// for backends that don't support multishot but rely on caller resubmission.
  fn push_stream(&mut self, id: u64, op: Op) -> io::Result<()> {
    self.push(id, op)
  }

  /// Cancels an in-flight operation.
  ///
  /// This is used to cancel streaming operations when the stream is dropped.
  /// For io_uring, this uses IORING_OP_ASYNC_CANCEL.
  /// For pollingv2, this removes the fd registration and stream op tracking.
  ///
  /// The default implementation is a no-op (for backends where operations
  /// complete synchronously or don't need explicit cancellation).
  fn cancel(&mut self, _id: u64) -> io::Result<()> {
    Ok(())
  }
}
