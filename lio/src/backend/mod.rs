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
//! use lio::backend::{IoBackend, op::Op, impls::Poller};
//! use std::time::Duration;
//!
//! // Create and initialize backend
//! let mut backend = Poller::default();
//! backend.init(1024).unwrap();  // Pre-allocate for 1024 concurrent ops
//!
//! // Submit a nop operation
//! backend.push(1, Op::Nop, &mut bumpalo::Bump::new());
//! backend.flush().unwrap();
//!
//! // Poll for completions (non-blocking)
//! let mut completions = Vec::new();
//! backend.wait(Some(Duration::ZERO), &mut completions).unwrap();
//! ```

pub mod ds;
pub mod op;

/// Test utilities for IoBackend implementations.
///
/// Use the `test_io_backend!` macro to generate a comprehensive test suite
/// for your IoBackend implementation.
#[macro_use]
pub mod test_macro;

#[cfg(feature = "backend_impls")]
pub mod impls {

  // #[cfg(test)]
  // pub mod dummy;

  #[cfg(target_os = "linux")]
  mod io_uring;

  #[cfg(target_os = "linux")]
  pub use io_uring::IoUring;

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
  mod pollingv2;
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
  pub use pollingv2::Poller;

  #[cfg(windows)]
  mod iocp;
  #[cfg(windows)]
  pub use iocp::*;

  pub(crate) mod sockaddr;
}

pub(crate) mod store;

use std::io;
use std::time::Duration;

use bumpalo::Bump;

use crate::backend::op::Op;

/// Represents a completed I/O operation.
///
/// This type is returned by backend handlers when an operation completes,
/// containing the operation ID and its result.
#[derive(Debug)]
pub struct OpCompleted {
  registration_id: u64,
  result: isize,
}

impl OpCompleted {
  /// Creates a new completed operation result.
  ///
  /// # Parameters
  ///
  /// - `op_id`: The unique ID of the operation
  /// - `result`: The operation result (non-negative for success, negative errno for error)
  pub fn new(op_id: u64, result: isize) -> Self {
    Self { registration_id: op_id, result }
  }

  pub fn result(&self) -> isize {
    self.result
  }

  pub fn registration_id(&self) -> u64 {
    self.registration_id
  }
}

/// Unified I/O backend trait to drive `OpModel`'s to completion.
///
/// Designed for single-thread ownership (`&mut self`), dyn-compatible for
/// runtime backend selection via `Box<dyn IoBackend>`.
///
/// Contract:
/// - `init()` must be called before `push()`, `flush()`, or `wait()`
/// - `push()` only queues work locally; queued operations are not observable
///   until `flush()` submits them
/// - `flush()` submits all currently queued operations and may also make
///   immediate completions observable on the next `wait()`
/// - `wait()` writes zero or more completions into the caller-provided
///   `completed` vector for that call only
///
/// # Usage
///
/// ```ignore
/// let mut backend = Backend::default();
/// backend.init(1024)?;                    // Pre-allocate for 1024 ops
/// backend.push(op_id, op)?;               // Queue operation
/// backend.flush()?;                       // Submit to kernel
/// let done = backend.wait(timeout)?;      // Get completions
/// ```
pub trait IoBackend {
  /// Initialize backend with capacity for `cap` concurrent operations.
  ///
  /// Pre-allocates all resources for zero-allocation runtime.
  /// Must be called once before any other methods.
  fn init(&mut self, cap: usize) -> io::Result<()>;

  /// Queue an operation for submission.
  ///
  /// Call `flush()` to submit queued operations.
  fn push(&mut self, id: u64, op: Op, step_bump: &mut Bump);

  /// Submit all queued operations to kernel.
  fn flush(&mut self) -> io::Result<()>;

  /// Wait for completions with optional timeout.
  ///
  /// - `None` = block until at least one completion
  /// - `Some(ZERO)` = non-blocking poll
  /// - `Some(duration)` = wait up to duration
  /// - `completed` is caller-owned output storage; implementations may clear
  ///   and rewrite it on each call
  fn wait(
    &mut self,
    timeout: Option<Duration>,
    completed: &mut Vec<OpCompleted>,
  ) -> io::Result<()>;
}
