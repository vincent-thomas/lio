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
//! use lio::backend::{IoBackend, op::Op, impls::DefaultBackend};
//! use std::time::Duration;
//!
//! // Create and initialize backend
//! let mut backend = DefaultBackend::new();
//! backend.init(1024).unwrap();  // Pre-allocate for 1024 concurrent ops
//!
//! // Submit a nop operation. Its arena must outlive the operation.
//! let mut step_bump = bumpalo::Bump::new();
//! // SAFETY: `Op::Nop` points at nothing, and id 1 is not in flight.
//! unsafe { backend.push(1, Op::Nop, &mut step_bump) };
//! backend.flush().unwrap();
//!
//! // Poll for completions (non-blocking)
//! let mut completions = Vec::new();
//! backend.wait(Some(Duration::ZERO), &mut completions).unwrap();
//! ```

pub mod ds;
pub mod op;
pub mod testing;

/// Test utilities for IoBackend implementations.
///
/// Use the `test_io_backend!` macro to generate a comprehensive test suite
/// for your IoBackend implementation.
#[macro_use]
pub mod test_macro;

#[cfg(feature = "backend_impls")]
pub mod impls {
  #[cfg(unix)]
  #[macro_use]
  pub(crate) mod syscall;

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

  pub(crate) use super::sockaddr;
  #[cfg(unix)]
  pub(crate) mod unix_stat;

  /// The backend [`Lio::new`](crate::Lio::new) uses on this platform.
  #[cfg(target_os = "linux")]
  pub type DefaultBackend = IoUring;

  /// The backend [`Lio::new`](crate::Lio::new) uses on this platform.
  #[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "tvos",
    target_os = "watchos",
    target_os = "freebsd",
    target_os = "dragonfly",
    target_os = "openbsd",
    target_os = "netbsd"
  ))]
  pub type DefaultBackend = Poller;

  /// The backend [`Lio::new`](crate::Lio::new) uses on this platform.
  #[cfg(windows)]
  pub type DefaultBackend = Iocp;
}

pub(crate) mod store;

/// The host OS's native close and standard handles.
#[path = "impls/native.rs"]
pub(crate) mod native;

/// OS socket-address conversions. Lives with the backend implementations but
/// is also needed by `net` when no backend implementations are compiled in.
#[path = "impls/sockaddr.rs"]
#[cfg_attr(not(feature = "backend_impls"), allow(dead_code))]
pub(crate) mod sockaddr;

use std::io;
use std::time::Duration;

use bumpalo::Bump;

use crate::api::resource::CloseFn;
use crate::backend::op::Op;

/// Represents a completed I/O operation.
///
/// This type is returned by backend handlers when an operation completes,
/// containing the operation ID and its result.
#[derive(Debug, Clone, Copy)]
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
  #[inline]
  pub fn new(op_id: u64, result: isize) -> Self {
    Self { registration_id: op_id, result }
  }

  #[inline]
  pub fn result(&self) -> isize {
    self.result
  }

  #[inline]
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
/// unsafe { backend.push(op_id, op, &mut step_bump) }; // Queue operation
/// backend.flush()?;                       // Submit to kernel
/// let done = backend.wait(timeout)?;      // Get completions
/// ```
///
/// # Safety
///
/// lio pushes [`Op`]s whose raw pointers point into memory owned by op
/// models, frees or reuses that memory once the operation's completion is
/// reported, and acts on what the backend reports without checking it.
/// Implementors must guarantee:
///
/// - **One completion per push.** Each `push(id, ..)` is followed by exactly
///   one [`OpCompleted`] for `id` from [`wait`](Self::wait), unless
///   [`drain`](Self::drain) returns `true` first, also when the operation
///   was [`cancel`](Self::cancel)led. `wait` never reports an `id` that is
///   not pushed and still unreported: lio pushes an `id` again once its
///   completion arrives, so a stray or duplicate completion would complete a
///   different operation whose memory is still in use.
/// - **No access after completion.** Neither the backend, nor the kernel, nor
///   any thread working for it reads or writes memory an `Op` points to, or
///   allocations made in the `step_bump` passed to its `push`, after the
///   operation's completion has been reported. A completion is only reported
///   once the operation has really finished.
/// - **Honest drain.** `drain` returns `true` only when no pushed operation
///   can still access that memory. lio drops the backend before freeing it,
///   so the backend's own `Drop` is the only code that may still touch it.
/// - **Truthful results.** A completion's result, and anything written
///   through the `Op`'s pointers, mean what the `Op` variant documents. In
///   particular:
///   - a successful result of an operation that creates a resource
///     ([`Op::Socket`], [`Op::Accept`], [`Op::OpenAt`]) is a newly created
///     descriptor/handle whose ownership the backend transfers to lio: nothing
///     else owns it or will close it;
///   - a successful byte count is at most the number of bytes actually
///     written, in order, into the operation's buffers (for reads) or taken
///     from them (for writes);
///   - output storage the `Op` points to (`FileStat`, `OsString`,
///     `SocketAddrBuf`, `ReadDirResult`, ...) holds valid, initialized values
///     on success, and a `ReadDir` state/drop-function pair stays sound to
///     call once, from any thread.
/// - **Matching close.** [`resource_close`](Self::resource_close) releases
///   exactly the resources this backend's completions carry, from any thread.
///
/// A safe `impl` does not compile, so safe code cannot hand lio descriptors
/// it does not own, or have lio free memory still in use:
///
/// ```compile_fail,E0200
/// use lio::backend::{IoBackend, OpCompleted, op::Op};
/// use std::{io, time::Duration};
///
/// struct Liar;
///
/// impl IoBackend for Liar {
///   fn init(&mut self, _: usize) -> io::Result<()> {
///     Ok(())
///   }
///   unsafe fn push(&mut self, _: u64, _: Op, _: &mut bumpalo::Bump) {}
///   fn flush(&mut self) -> io::Result<()> {
///     Ok(())
///   }
///   fn wait(
///     &mut self,
///     _: Option<Duration>,
///     _: &mut Vec<OpCompleted>,
///   ) -> io::Result<()> {
///     Ok(())
///   }
/// }
/// ```
pub unsafe trait IoBackend {
  /// Initialize backend with capacity for `cap` concurrent operations.
  ///
  /// Pre-allocates all resources for zero-allocation runtime.
  /// Must be called once before any other methods.
  fn init(&mut self, cap: usize) -> io::Result<()>;

  /// How resources produced by this backend's completions are closed.
  ///
  /// The default closes them as native OS descriptors/handles. A backend
  /// whose completions do not carry real OS resources (such as a simulation)
  /// must override this, or dropping those resources would close unrelated
  /// descriptors of the process.
  fn resource_close(&self) -> CloseFn {
    native::close
  }

  /// Queue an operation for submission.
  ///
  /// Call `flush()` to submit queued operations.
  ///
  /// # Safety
  ///
  /// The caller guarantees that:
  /// - every pointer in `op` is valid as the [`OpModel`] safety contract
  ///   describes, and stays so until this backend reports the completion
  ///   for `id`, or until [`drain`](Self::drain) returns `true`;
  /// - `step_bump` is not reset or dropped before then either, since the
  ///   backend may lower the operation into it;
  /// - `id` is not in flight already: a previous `push` with the same `id`
  ///   has had its completion reported, or was drained.
  ///
  /// Calling it from safe code does not compile:
  ///
  /// ```compile_fail,E0133
  /// use lio::backend::{IoBackend, impls::DefaultBackend, op::Op};
  ///
  /// let mut backend = DefaultBackend::new();
  /// backend.push(1, Op::Nop, &mut bumpalo::Bump::new());
  /// ```
  ///
  /// [`OpModel`]: crate::api::op::OpModel
  unsafe fn push(&mut self, id: u64, op: Op, step_bump: &mut Bump);

  /// Submit all queued operations to kernel.
  fn flush(&mut self) -> io::Result<()>;

  /// Asks the backend to finish operation `id` early: its consumer is gone.
  ///
  /// The operation must still produce exactly one completion, from a later
  /// `wait()`: a cancellation error, or its own result if it finished
  /// first. Until then the memory its [`Op`] points to stays valid, so the
  /// backend must not consider the operation gone before it reports it.
  ///
  /// `id` may be unknown to the backend (already completed, or not an I/O
  /// operation); that is not an error.
  ///
  /// The default does nothing: the operation runs to completion, and its
  /// slot is freed then. Override it so operations that may never complete
  /// on their own (an accept, a read from an idle socket) do not keep their
  /// slot forever.
  fn cancel(&mut self, id: u64) {
    let _ = id;
  }

  /// Cancels every operation still running and waits, for a bounded time,
  /// until none is left.
  ///
  /// Returns whether nothing can touch the memory of a pushed [`Op`] any
  /// more: no request is left in the kernel or on a backend thread. The
  /// driver calls this when it is dropped, before freeing that memory, and
  /// leaks the memory and the backend instead when it returns `false`.
  /// Completions for the drained operations are discarded.
  ///
  /// The default returns `true`, which is right for a backend that only
  /// performs I/O inside its own calls. A backend that leaves work in the
  /// kernel or on other threads between calls must override it.
  fn drain(&mut self) -> bool {
    true
  }

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
