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
//! let op = unsafe { InFlight::attest(BorrowedOp::nop()) };
//! backend.push(1, op, &mut bumpalo::Bump::new());
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

  #[cfg(target_os = "wasi")]
  pub(crate) mod wasi;
  #[cfg(target_os = "wasi")]
  pub use wasi::Wasi;

  #[cfg(any(unix, windows))]
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

  /// The backend [`Lio::new`](crate::Lio::new) uses on this platform.
  #[cfg(target_os = "wasi")]
  pub type DefaultBackend = Wasi;
}

pub(crate) mod store;

/// The host OS's native close and standard handles.
#[path = "impls/native.rs"]
pub(crate) mod native;

/// OS socket-address conversions. Lives with the backend implementations but
/// is also needed by `net` when no backend implementations are compiled in.
#[cfg(any(unix, windows))]
#[path = "impls/sockaddr.rs"]
#[cfg_attr(not(feature = "backend_impls"), allow(dead_code))]
pub(crate) mod sockaddr;

/// Socket-address queries on WASI, where a socket is a typed handle held by
/// the WASI backend instead of a descriptor the OS can be asked about.
#[cfg(target_os = "wasi")]
pub(crate) mod sockaddr {
  /// Looks up the local (`peer == false`) or remote address of a socket.
  #[cfg(feature = "high")]
  pub(crate) fn socket_name(
    socket: &crate::api::resource::Resource,
    peer: bool,
  ) -> std::io::Result<std::net::SocketAddr> {
    // The WASI backend does not create sockets yet.
    let _ = (socket, peer);
    Err(std::io::Error::from(std::io::ErrorKind::Unsupported))
  }
}

use std::io;
use std::time::Duration;

use bumpalo::Bump;

use crate::api::resource::ResourceClose;
use crate::backend::op::{BorrowedOp, Op};

/// Represents a completed I/O operation.
///
/// This type is returned by backend handlers when an operation completes,
/// containing the operation ID and its result.
#[derive(Debug, Clone, Copy)]
pub struct OpCompleted {
  registration_id: u64,
  result: isize,
  cancelled: bool,
}

impl OpCompleted {
  /// Creates a new completed operation result.
  ///
  /// # Parameters
  ///
  /// - `op_id`: The unique ID of the operation
  /// - `result`: The operation result (non-negative for success, negative errno for error)
  ///
  /// The driver trusts `result` once [`IoBackend::wait`] reports it. See the
  /// safety contract of [`IoBackend`].
  #[inline]
  pub fn new(op_id: u64, result: isize) -> Self {
    Self { registration_id: op_id, result, cancelled: false }
  }

  /// An operation that was stopped by [`IoBackend::cancel`] before it
  /// finished.
  ///
  /// `result` is the error the platform reports for that, which differs
  /// between platforms. [`is_cancelled`](Self::is_cancelled) is what says
  /// so on all of them.
  #[inline]
  pub fn cancelled(op_id: u64, result: isize) -> Self {
    Self { registration_id: op_id, result, cancelled: true }
  }

  /// Whether the operation was stopped by [`IoBackend::cancel`].
  ///
  /// `false` for one that finished first, even after a cancel was asked
  /// for: it reports its own result.
  #[inline]
  pub fn is_cancelled(&self) -> bool {
    self.cancelled
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
/// # Safety
///
/// The driver frees an operation's buffers based on what the backend reports.
/// The compiler cannot check that, so the implementor must uphold it:
///
/// - Store the [`Keep`] from [`push`](IoBackend::push) for as long as this
///   backend, the kernel, or a worker thread may use an address copied from
///   that [`Op`].
/// - Report each pushed operation at most once from [`wait`](IoBackend::wait),
///   and only after nothing outside the backend can use its addresses.
/// - Report only what happened. The driver reads each result into the
///   operation's memory as proof, without checking it: a non-negative result for an operation
///   that fills memory means that many bytes of it were written, and for an
///   operation that creates a resource it is a new descriptor or handle that
///   nothing else owns, closed by [`resource_close`](IoBackend::resource_close).
/// - Return `true` from [`drain`](IoBackend::drain) only when the kernel and
///   every worker thread are finished with every pushed address.
pub unsafe trait IoBackend {
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
  /// `op` borrows the memory that owns its buffers. [`Lio`](crate::Lio)
  /// builds it with [`InFlight::from_model`](InFlight::from_model) while that
  /// memory is borrowed. Take [`InFlight::into_parts`] and store the [`Keep`]
  /// for as long as any address copied from the [`Op`] is used. The driver
  /// frees the model when it holds the last [`Keep`]. [`drain`](IoBackend::drain)
  /// still has to stop kernel and worker threads, which do not hold a
  /// [`Keep`].
  fn push(&mut self, id: u64, op: InFlight<'_>, step_bump: &mut Bump);

  /// Submit all queued operations to kernel.
  fn flush(&mut self) -> io::Result<()>;

  /// Asks the backend to finish operation `id` early: its consumer is gone.
  ///
  /// The operation must still produce exactly one completion, from a later
  /// `wait()`: an [`OpCompleted::cancelled`], or its own result if it
  /// finished first. Until then the memory its [`Op`] points to stays valid, so the
  /// backend must not consider the operation gone before it reports it.
  ///
  /// `id` may be unknown to the backend (already completed, or not an I/O
  /// operation); that is not an error.
  ///
  /// A backend may do nothing here only if every operation it accepts
  /// finishes on its own. One that may never complete (an accept, a read
  /// from an idle socket) would otherwise keep its slot forever.
  fn cancel(&mut self, id: u64);

  /// Drops every stored [`Keep`] and waits, for a bounded time, until nothing
  /// outside this call can dereference the operations.
  ///
  /// Returns whether the kernel and any worker threads are finished. The
  /// driver drops the backend after `true`, which drops the [`Keep`]s it
  /// stored, then frees a model only when no [`Keep`] remains. It leaks the
  /// backend and the models when this returns `false`, because a thread may
  /// still be writing. Completions for the drained operations are discarded.
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
/// The driver holds one. Each copy a backend stores holds another. The
/// driver frees the model only when it holds the last one. Dropping a copy
/// early, while a raw pointer taken from the operation is still in use, is
/// still a use after free. Forgetting a copy leaks the model.
#[derive(Clone, Debug)]
pub struct Keep {
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

/// One operation borrowed from the memory that owns its buffers.
///
/// [`Lio`](crate::Lio) builds this with [`from_model`](Self::from_model)
/// while that model is borrowed, then passes it to [`IoBackend::push`]. The
/// model stays pinned in the driver until the last [`Keep`] is dropped. A
/// buffer that lives only on the caller's stack cannot be turned into an
/// `InFlight` without [`attest`](Self::attest), because nothing would keep
/// it alive after the call returned.
pub struct InFlight<'a> {
  op: BorrowedOp<'a>,
  keep: Keep,
}

impl<'a> InFlight<'a> {
  /// Wraps an operation borrowed from a model the driver has pinned.
  ///
  /// The driver calls this while it borrows the operation's memory to build
  /// the action, and does not move or drop that memory until the backend is
  /// finished with it.
  pub(crate) fn from_model(op: BorrowedOp<'a>, keep: Keep) -> Self {
    Self { op, keep }
  }

  /// Wraps an operation whose buffers the caller keeps alive itself.
  ///
  /// Contract tests use this. [`Lio`](crate::Lio) does not: it submits the
  /// model it owns, through [`from_model`](Self::from_model).
  ///
  /// # Safety
  ///
  /// The addresses in `op` stay allocated and unmoved, and nothing but the
  /// backend mutates them, until every [`Keep`] from [`into_parts`](Self::into_parts)
  /// is dropped. This keep is not tied to a driver registration.
  pub unsafe fn attest(op: BorrowedOp<'a>) -> Self {
    Self { op, keep: Keep::new() }
  }

  /// The operation the backend copies addresses out of.
  #[inline]
  pub fn as_op(&self) -> &Op {
    self.op.as_op()
  }

  /// The operation and the lease that keeps its buffers allocated.
  ///
  /// Store the [`Keep`] with every copy of the addresses. Drop it when those
  /// copies are gone.
  #[inline]
  pub fn into_parts(self) -> (Op, Keep) {
    (self.op.as_op().clone(), self.keep)
  }
}
