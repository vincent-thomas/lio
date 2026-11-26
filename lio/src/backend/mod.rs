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
//! use lio::backend::{InFlight, IoBackend, op::BorrowedOp, impls::DefaultBackend};
//! use std::time::Duration;
//!
//! let mut backend = DefaultBackend::new();
//! backend.init(1024).unwrap();
//!
//! // A nop borrows nothing. Real submissions borrow an operation's memory,
//! // which the driver keeps in place: `Lio` calls `InFlight::from_model`
//! // while that memory is borrowed.
//! // SAFETY: this nop has no buffer for the backend to retain.
//! let op = unsafe { InFlight::attest(1, BorrowedOp::nop()) };
//! backend.push(op, &mut bumpalo::Bump::new());
//! backend.flush().unwrap();
//!
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
  pub(crate) mod iovec;
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

use crate::api::resource::ResourceClose;
use crate::backend::op::{BorrowedOp, Op};

/// One finished operation: what [`IoBackend::wait`] reports.
///
/// It is made from the operation's [`Ticket`] and nothing else, so a backend
/// cannot report an operation it was not given, or report one twice. It
/// carries the operation's lease: the driver frees the operation's memory
/// once it has read this.
#[derive(Debug)]
pub struct OpCompleted {
  registration_id: u64,
  result: isize,
  cancelled: bool,
  _keep: Keep,
}

impl OpCompleted {
  /// Non-negative for success, a negative errno for an error.
  #[inline]
  pub fn result(&self) -> isize {
    self.result
  }

  #[inline]
  pub fn registration_id(&self) -> u64 {
    self.registration_id
  }

  /// Whether the operation was stopped by [`IoBackend::cancel`].
  ///
  /// `false` for one that finished first, even after a cancel was asked
  /// for: it reports its own result.
  #[inline]
  pub fn is_cancelled(&self) -> bool {
    self.cancelled
  }

  /// A completion no backend was given a ticket for, to test what the
  /// driver does with one.
  #[cfg(test)]
  pub(crate) fn forged(op_id: u64, result: isize) -> Self {
    Self {
      registration_id: op_id,
      result,
      cancelled: false,
      _keep: Keep::new(),
    }
  }
}

/// Unified I/O backend trait: executes the operations the driver submits.
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
/// [`Lio`](crate::Lio) is what submits. It owns the operation's memory,
/// buffers included. While that memory is borrowed, the driver builds an
/// [`InFlight`] and passes it to [`push`](IoBackend::push). The borrow is
/// what ties the operation to the memory the backend may keep using until
/// the completion comes back.
///
/// # What an implementor promises
///
/// The trait is safe to implement. The driver frees an operation's buffers
/// based on what the backend reports, and the two things it cannot check
/// are claimed where they are made, by calling an `unsafe` method of the
/// operation's [`Ticket`]:
///
/// - reporting or releasing it says nothing outside the backend (the
///   kernel, a worker thread) uses an address copied from that [`Op`] any
///   more;
/// - [`Ticket::complete`] says the result is what happened.
///
/// A ticket that is dropped without either leaks the operation's memory:
/// forgetting one is a leak, never a use after free.
pub trait IoBackend {
  /// Initialize backend with capacity for `cap` concurrent operations.
  ///
  /// Pre-allocates all resources for zero-allocation runtime.
  /// Must be called once before any other methods.
  fn init(&mut self, cap: usize) -> io::Result<()>;

  /// How resources produced by this backend's completions are closed.
  ///
  /// [`ResourceClose::native`] closes them as OS descriptors or handles. A
  /// backend whose completions do not carry real OS resources (such as a
  /// simulation) must not return that, or dropping those resources would
  /// close unrelated descriptors of the process.
  fn resource_close(&self) -> ResourceClose;

  /// Queue an operation for submission.
  ///
  /// Call `flush()` to submit queued operations.
  ///
  /// `op` borrows the memory that owns its buffers. Take
  /// [`InFlight::into_parts`]: the [`Op`] to act on, and the [`Ticket`] to
  /// keep until the operation is reported from [`wait`](IoBackend::wait).
  fn push(&mut self, op: InFlight<'_>, step_bump: &mut Bump);

  /// Submit all queued operations to kernel.
  fn flush(&mut self) -> io::Result<()>;

  /// Asks the backend to finish operation `id` early: its consumer is gone.
  ///
  /// The operation must still produce exactly one completion, from a later
  /// `wait()`: [`Ticket::cancelled`], or its own result if it finished
  /// first. Until then the memory its [`Op`] points to stays valid.
  ///
  /// `id` may be unknown to the backend (already completed, or not an I/O
  /// operation); that is not an error.
  ///
  /// A backend may do nothing here only if every operation it accepts
  /// finishes on its own. One that may never complete (an accept, a read
  /// from an idle socket) would otherwise keep its slot forever.
  fn cancel(&mut self, id: u64);

  /// Stops what is in flight and waits, for a bounded time, until nothing
  /// outside this call can dereference the operations. Each one that is
  /// finished is given up with [`Ticket::release`].
  ///
  /// Returns whether the kernel and any worker threads are finished with
  /// everything. The driver drops the backend after `true`, and leaks it
  /// after `false`. Tickets still held either way are dropped unreported,
  /// which leaks their operations' memory instead of freeing it under a
  /// thread that may still be writing. Completions for the drained
  /// operations are discarded.
  ///
  /// Returning `true` at once is right only for a backend that leaves no
  /// work in the kernel or on other threads between calls.
  fn drain(&mut self) -> bool;

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

/// One share of the lifetime of an operation's memory.
///
/// The driver holds one. A [`Ticket`], and then the [`OpCompleted`] made
/// from it, holds another. The driver frees the model only when it holds
/// the last one.
#[derive(Clone, Debug)]
pub(crate) struct Keep {
  alive: std::sync::Arc<()>,
}

impl Keep {
  pub(crate) fn new() -> Self {
    Self { alive: std::sync::Arc::new(()) }
  }

  /// Another owner still has a copy.
  pub(crate) fn held_elsewhere(&self) -> bool {
    std::sync::Arc::strong_count(&self.alive) > 1
  }
}

/// The right to report one operation, and the lease on its memory.
///
/// A backend gets one per operation from [`InFlight::into_parts`], keeps it
/// while the operation is in flight, and gives it up exactly once: to
/// report the operation, or to [`release`](Self::release) it unreported.
///
/// Dropping a ticket leaks the operation's memory. That is the safe thing
/// to happen to an operation the kernel may still be writing to.
#[derive(Debug)]
pub struct Ticket {
  id: u64,
  keep: std::mem::ManuallyDrop<Keep>,
}

impl Ticket {
  /// The id the driver knows the operation by, as [`IoBackend::cancel`]
  /// names it.
  #[inline]
  pub fn id(&self) -> u64 {
    self.id
  }

  #[inline]
  fn into_keep(mut self) -> Keep {
    // SAFETY: `self` is consumed here and its `Drop` does nothing, so the
    // lease is taken out exactly once.
    unsafe { std::mem::ManuallyDrop::take(&mut self.keep) }
  }

  /// Reports the operation with `result`: non-negative for success, a
  /// negative errno for an error.
  ///
  /// # Safety
  ///
  /// - Nothing outside the caller (the kernel, a worker thread) uses an
  ///   address copied from the operation's [`Op`] any more.
  /// - `result` is what happened. The driver reads it into the operation's
  ///   memory without checking: a non-negative result for an operation that
  ///   fills memory means that many bytes of it were written, and for one
  ///   that creates a resource it is a new descriptor or handle nothing else
  ///   owns, which [`IoBackend::resource_close`] closes.
  #[inline]
  pub unsafe fn complete(self, result: isize) -> OpCompleted {
    let registration_id = self.id;
    OpCompleted {
      registration_id,
      result,
      cancelled: false,
      _keep: self.into_keep(),
    }
  }

  /// Reports the operation as stopped by [`IoBackend::cancel`] before it
  /// finished. `result` is the error the platform has for that.
  ///
  /// # Safety
  ///
  /// Nothing outside the caller uses an address copied from the operation's
  /// [`Op`] any more, and `result` is negative.
  #[inline]
  pub unsafe fn cancelled(self, result: isize) -> OpCompleted {
    let registration_id = self.id;
    OpCompleted {
      registration_id,
      result,
      cancelled: true,
      _keep: self.into_keep(),
    }
  }

  /// Gives the operation up without reporting it, from
  /// [`IoBackend::drain`].
  ///
  /// # Safety
  ///
  /// Nothing outside the caller uses an address copied from the operation's
  /// [`Op`] any more.
  #[inline]
  pub unsafe fn release(self) {
    drop(self.into_keep());
  }
}

/// One operation borrowed from the memory that owns its buffers.
///
/// [`Lio`](crate::Lio) builds this while that memory is borrowed, then
/// passes it to [`IoBackend::push`]. The memory stays in place until the
/// operation's [`Ticket`] is given up. A buffer that lives only on the
/// caller's stack cannot be turned into an `InFlight` without
/// [`attest`](Self::attest), because nothing would keep it alive after the
/// call returned.
pub struct InFlight<'a> {
  op: BorrowedOp<'a>,
  ticket: Ticket,
}

impl<'a> InFlight<'a> {
  /// Wraps an operation borrowed from a model the driver has pinned.
  ///
  /// The driver calls this while it borrows the operation's memory to build
  /// the action, and does not move or drop that memory until the backend
  /// has given the ticket up.
  pub(crate) fn from_model(id: u64, op: BorrowedOp<'a>, keep: Keep) -> Self {
    Self { op, ticket: Ticket { id, keep: std::mem::ManuallyDrop::new(keep) } }
  }

  /// Puts an operation and its ticket back together, to hand both on to
  /// another backend.
  pub(crate) fn from_parts(op: BorrowedOp<'a>, ticket: Ticket) -> Self {
    Self { op, ticket }
  }

  /// Wraps an operation whose buffers the caller keeps alive itself.
  ///
  /// Contract tests use this. [`Lio`](crate::Lio) does not: it submits the
  /// model it owns.
  ///
  /// # Safety
  ///
  /// The addresses in `op` stay allocated and unmoved, and nothing but the
  /// backend mutates them, until the [`Ticket`] from
  /// [`into_parts`](Self::into_parts) has been given up and the
  /// [`OpCompleted`] made from it dropped. This lease is not tied to a
  /// driver registration.
  pub unsafe fn attest(id: u64, op: BorrowedOp<'a>) -> Self {
    Self::from_model(id, op, Keep::new())
  }

  /// The id the driver knows the operation by.
  #[inline]
  pub fn id(&self) -> u64 {
    self.ticket.id
  }

  /// The operation the backend copies addresses out of.
  #[inline]
  pub fn as_op(&self) -> &Op {
    self.op.as_op()
  }

  /// The operation, and the ticket to report it with.
  ///
  /// Keep the [`Ticket`] with every copy of the addresses, and give it up
  /// only once those copies are gone.
  #[inline]
  pub fn into_parts(self) -> (Op, Ticket) {
    (self.op.as_op().clone(), self.ticket)
  }
}
