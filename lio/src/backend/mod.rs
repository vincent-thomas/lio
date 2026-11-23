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
//! use lio::backend::{drive, IoBackend, op::BorrowedOp, impls::DefaultBackend};
//! use std::time::Duration;
//!
//! // Create and initialize backend
//! let mut backend = DefaultBackend::new();
//! backend.init(1024).unwrap(); // Pre-allocate for 1024 concurrent ops
//!
//! // `drive` lends the backend for one scope. Buffers borrowed by an
//! // operation stay borrowed until the scope ends, which is after `drain`.
//! let completions = drive(&mut backend, |submit| {
//!     submit.push(1, BorrowedOp::nop(), &mut bumpalo::Bump::new());
//!     submit.flush().unwrap();
//!     let mut completions = Vec::new();
//!     submit.wait(Some(Duration::ZERO), &mut completions).unwrap();
//!     completions
//! });
//! let _ = completions;
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
use std::marker::PhantomData;
use std::process;
use std::time::Duration;

use bumpalo::Bump;

use crate::api::resource::ResourceClose;
use crate::backend::op::BorrowedOp;

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
/// Submit through [`drive`]. It is safe, and it is what keeps a buffer
/// alive for as long as the backend may still hold its address. `push` is
/// the unsafe primitive [`drive`] and [`Lio`](crate::Lio) call.
///
/// ```ignore
/// let mut backend = Backend::default();
/// backend.init(1024)?;
/// let done = lio::backend::drive(&mut backend, |submit| {
///     submit.push(op_id, op, &mut bump);
///     submit.flush()?;
///     let mut done = Vec::new();
///     submit.wait(timeout, &mut done)?;
///     Ok(done)
/// })?;
/// ```
pub trait IoBackend {
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
  fn resource_close(&self) -> ResourceClose {
    ResourceClose::native()
  }

  /// Queue an operation for submission.
  ///
  /// Call `flush()` to submit queued operations.
  ///
  /// # Safety
  ///
  /// The backend may copy addresses out of `op` and dereference them after
  /// `push` returns, from `flush`, `wait`, `cancel`, or `drain`. A lifetime
  /// that ended when `push` returned cannot say that, so this method stays
  /// `unsafe`. Those addresses must stay allocated and unmoved, and must not
  /// be mutated by anyone but this backend, until `wait` returns this
  /// operation's completion or `drain` returns `true`.
  ///
  /// [`drive`] upholds that by borrowing the buffers for the whole scope and
  /// calling `drain` before the scope ends. [`Lio`](crate::Lio) upholds it
  /// for operations its models return: `action` borrows the model, the
  /// registration stays pinned until the backend is done, and the driver
  /// does not ask the model for another action before then. An operation
  /// submitted by the driver outlives the `try_run` that queued it, so the
  /// driver calls `push` rather than wrapping each turn in [`drive`].
  unsafe fn push(&mut self, id: u64, op: BorrowedOp<'_>, step_bump: &mut Bump);

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

  /// Drops every address copied out of a pushed operation and waits, for a
  /// bounded time, until nothing outside this call can dereference them.
  ///
  /// Returns whether that is true. Stored copies must be dropped before this
  /// returns `true`. The driver calls this when it is dropped, before
  /// freeing that memory, and leaks the memory and the backend instead when
  /// it returns `false`. [`drive`] calls it before the buffers borrowed for
  /// the scope are released, and aborts the process when it returns `false`:
  /// unwinding would free those buffers while the backend still held them.
  /// Completions for the drained operations are discarded.
  ///
  /// The default returns `true` immediately, which is right only for a
  /// backend that never copies an address out of the [`BorrowedOp`] it was
  /// given. A backend that stores those addresses, or that leaves work in
  /// the kernel or on other threads between calls, must override this.
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

/// One submission scope of [`drive`].
///
/// `'scope` is invariant. A local of the closure cannot satisfy it, so every
/// buffer and descriptor array a [`BorrowedOp<'scope>`](BorrowedOp) borrows
/// has to belong to the caller of [`drive`]. `drive` drains the backend
/// before it gives those buffers back.
pub struct Submit<'scope, 'env: 'scope, B: ?Sized> {
  backend: &'scope mut B,
  // Invariant in both lifetimes, as `std::thread::Scope` is: a shorter
  // scope would let the closure borrow a local that drops before `drain`.
  _scope: PhantomData<&'scope mut &'scope ()>,
  _env: PhantomData<&'env mut &'env ()>,
}

impl<'scope, 'env, B> Submit<'scope, 'env, B>
where
  B: IoBackend + ?Sized,
{
  /// Queues `op`.
  ///
  /// `op` borrows its buffers for `'scope`. [`drive`] keeps that scope alive
  /// until [`IoBackend::drain`] returns `true`, which is the contract
  /// [`IoBackend::push`] requires.
  pub fn push(
    &mut self,
    id: u64,
    op: BorrowedOp<'scope>,
    step_bump: &mut Bump,
  ) {
    // SAFETY: `op` is borrowed for `'scope`, which a local of the closure
    // cannot satisfy. The memory belongs to the caller of `drive`, and
    // `drive` calls `drain` before returning that memory, including when
    // the closure panics. The closure holds `&mut Submit`, so it cannot end
    // the scope early.
    unsafe { self.backend.push(id, op, step_bump) }
  }

  /// Submits every operation queued in this scope.
  pub fn flush(&mut self) -> io::Result<()> {
    self.backend.flush()
  }

  /// Asks the backend to finish operation `id` early. See
  /// [`IoBackend::cancel`].
  pub fn cancel(&mut self, id: u64) {
    self.backend.cancel(id)
  }

  /// Polls for completions of operations queued in this scope. See
  /// [`IoBackend::wait`].
  pub fn wait(
    &mut self,
    timeout: Option<Duration>,
    completed: &mut Vec<OpCompleted>,
  ) -> io::Result<()> {
    self.backend.wait(timeout, completed)
  }
}

/// Runs `body` against `backend`, then waits until the backend has dropped
/// every address it copied.
///
/// This is the safe submission API. `body` may call [`Submit::push`] with a
/// [`BorrowedOp`] borrowed for the scope. The backend stores the addresses
/// in that operation — the bytes, and the descriptor array that points at
/// them — and may use them until it drains, which is after `body` returns.
/// That memory has to belong to the caller of `drive`. A local of `body`
/// does not live long enough, and `body` cannot return a reference into the
/// scope. Once `drive` returns, the caller can read and write the buffers
/// again.
///
/// The scope ends in `drive` itself, not in a guard the closure could
/// forget, so `mem::forget` inside `body` cannot release a buffer early.
///
/// # Panics
///
/// If `body` panics, the backend is still drained before the panic
/// continues. If [`IoBackend::drain`] returns `false`, the process aborts:
/// unwinding would drop the caller's buffers while something outside this
/// thread could still write them.
///
/// # Examples
///
/// ```
/// use lio::api::resource::Resource;
/// use lio::backend::drive;
/// use lio::backend::op::{BorrowedOp, RawBuf};
/// use lio::backend::testing::ScriptedBackend;
/// use lio::backend::IoBackend;
///
/// let mut backend = ScriptedBackend::auto(4).without_poison();
/// backend.init(1).unwrap();
/// let mut buf = [9u8; 4];
/// let n = {
///     let iov = [RawBuf::from_mut(&mut buf)];
///     drive(&mut backend, |submit| {
///         submit.push(
///             1,
///             BorrowedOp::read(
///                 Resource::invalid(),
///                 &iov,
///                 None,
///                 Default::default(),
///             ),
///             &mut bumpalo::Bump::new(),
///         );
///         submit.flush().unwrap();
///         let mut done = Vec::new();
///         submit.wait(None, &mut done).unwrap();
///         done[0].result()
///     })
/// };
/// assert_eq!(n, 4);
/// // The borrow ended with the scope, after the backend drained.
/// assert_eq!(buf, [9, 9, 9, 9]);
/// ```
///
/// A buffer that lives only inside the closure is rejected: it would be
/// dropped before the backend drained.
///
/// ```compile_fail
/// use lio::api::resource::Resource;
/// use lio::backend::drive;
/// use lio::backend::op::{BorrowedOp, RawBuf};
/// use lio::backend::testing::ScriptedBackend;
/// use lio::backend::IoBackend;
///
/// let mut backend = ScriptedBackend::new();
/// backend.init(1).unwrap();
/// drive(&mut backend, |submit| {
///     let mut buf = [0u8; 4];
///     let iov = [RawBuf::from_mut(&mut buf)];
///     submit.push(
///         1,
///         BorrowedOp::read(Resource::invalid(), &iov, None, Default::default()),
///         &mut bumpalo::Bump::new(),
///     );
/// });
/// ```
pub fn drive<'env, B, R>(
  backend: &'env mut B,
  body: impl for<'scope> FnOnce(&mut Submit<'scope, 'env, B>) -> R,
) -> R
where
  B: IoBackend + ?Sized,
{
  let mut guard = DrainOnDrop { backend, armed: true };
  let result = invoke(&mut guard, body);
  guard.armed = false;
  release(guard.backend);
  result
}

fn invoke<'env, B, R>(
  guard: &mut DrainOnDrop<'env, B>,
  body: impl for<'scope> FnOnce(&mut Submit<'scope, 'env, B>) -> R,
) -> R
where
  B: IoBackend + ?Sized,
{
  body(&mut Submit {
    backend: &mut *guard.backend,
    _scope: PhantomData,
    _env: PhantomData,
  })
}

fn release<B>(backend: &mut B)
where
  B: IoBackend + ?Sized,
{
  if !backend.drain() {
    process::abort();
  }
}

struct DrainOnDrop<'a, B: IoBackend + ?Sized> {
  backend: &'a mut B,
  armed: bool,
}

impl<B: IoBackend + ?Sized> Drop for DrainOnDrop<'_, B> {
  fn drop(&mut self) {
    if self.armed {
      release(self.backend);
    }
  }
}
