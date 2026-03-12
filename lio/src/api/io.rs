//! Operation progress tracking and result consumption.
//!
//! This module provides types for managing I/O operation lifecycles and consuming
//! their results through multiple patterns: async/await, callbacks, blocking calls,
//! and channels.
//!
//! # Overview
//!
//! The [`Io<T>`] type is the core abstraction that wraps an I/O operation and
//! provides various methods to consume its result:
//!
//! - **Async/await**: Implements `IntoFuture`, allowing direct `.await` syntax
//! - **Blocking**: [`wait()`](Io::wait) blocks until completion
//! - **Callbacks**: [`when_done()`](Io::when_done) executes a closure on completion
//! - **Channels**: [`send()`](Io::send) and [`send_with()`](Io::send_with)
//!   deliver results via channels
//!
//! # Architecture
//!
//! ```text
//! Io<T>
//!   ├─> IntoFuture ──> IoFuture<T> (async/await)
//!   ├─> wait()        (blocking)
//!   ├─> when_done(F)  (callback)
//!   ├─> send()    ──> Receiver<T>        (channel-based blocking)
//!   └─> send_with(Sender<T>)             (custom channel)
//! ```
//!
//! # Usage Patterns
//!
//! ```no_run
//! use lio::{Lio, api};
//! use std::os::fd::FromRawFd;
//!
//! let mut lio = Lio::new(64).unwrap();
//! let fd = unsafe { api::resource::Resource::from_raw_fd(1) };
//!
//! // Blocking via .wait()
//! let (result, buf) = api::write(&fd, vec![0; 10]).with_lio(&mut lio).wait();
//!
//! // Callbacks via .when_done()
//! api::write(&fd, vec![0; 10]).with_lio(&mut lio).when_done(|(result, buf)| {
//!     // Handle result
//! });
//!
//! // Channels via .send()
//! let receiver = api::write(&fd, vec![0; 10]).with_lio(&mut lio).send();
//! lio.try_run().unwrap();
//! let (result, buf) = receiver.recv();
//! ```
//!
//! # Thread Safety
//!
//! All types in this module are `Send`, allowing results to be consumed on different
//! threads than where operations were initiated. This is particularly useful for
//! delegating I/O completion handling to dedicated threads.

use crate::{lio, lio::Lio, registration::Registration, typed_op::TypedOp};

use std::{
  future::Future,
  pin::Pin,
  sync::mpsc as std_mpsc,
  task::{Context, Poll},
  time::Duration,
};

/// Represents an in-progress I/O operation with multiple consumption patterns.
///
/// [`Io<T>`] is the primary interface for consuming I/O operation results in lio.
/// It wraps an operation of type `T` and provides methods to retrieve the result through
/// various patterns suited to different programming models.
///
/// # Thread-Per-Core Design
///
/// In the thread-per-core model, each thread owns its own `Lio` instance. Use
/// `.with_lio()` to bind your Lio instance before consuming the operation:
///
/// ```no_run
/// use lio::{Lio, api};
/// use std::os::fd::FromRawFd;
///
/// let lio = Lio::new(1024).unwrap();
/// let fd = unsafe { api::resource::Resource::from_raw_fd(0) };
/// let (result, buf) = api::read(&fd, vec![0u8; 1024])
///     .with_lio(&lio)
///     .wait();
/// ```
///
/// # Examples
/// See examples in each method's docs.
#[must_use = "Io doesn't schedule any operation on itself."]
pub struct Io<T>
where
  T: Send,
{
  op: T,
  handle: LioHandle,
}

impl<T> Io<T>
where
  T: TypedOp,
{
  /// Block the current thread until the operation completes and return the result.
  ///
  /// # Deprecated
  ///
  /// This method is deprecated because it requires someone else to run the event loop
  /// (e.g., a background thread calling `lio.run()`). Use `.send()` with manual event
  /// loop management, or `.when_done()` for callback-based completion instead.
  #[inline]
  #[deprecated(
    since = "0.4.0",
    note = "wait() requires external event loop management. Use .send() with manual lio.run() calls or .when_done() instead."
  )]
  pub fn wait(self) -> T::Result
  where
    T::Result: Send,
  {
    self.send().recv()
  }
  /// Convert the operation into a channel receiver.
  ///
  /// Returns a [`Receiver`] which receives the operation result when complete.
  /// Useful for integrating with channel-based async code or when you need to wait
  /// for the result in a different context than where the operation was started.
  ///
  /// # Example
  /// ```no_run
  /// use lio::{Lio, api};
  /// use std::os::fd::FromRawFd;
  ///
  /// let lio = Lio::new(64).unwrap();
  /// let fd = unsafe { api::resource::Resource::from_raw_fd(1) };
  /// let receiver = api::write(&fd, vec![0; 10]).with_lio(&lio).send();
  /// lio.try_run().unwrap();
  /// let (result, buffer) = receiver.recv();
  /// ```
  #[inline]
  pub fn send(self) -> Receiver<T::Result>
  where
    T::Result: Send,
  {
    let (sender, receiver) = std_mpsc::channel();

    self.send_with(sender);

    Receiver { recv: Some(receiver) }
  }

  /// Sends the operation result through a provided channel sender when complete.
  ///
  /// # Examples
  ///
  /// ```rust,no_run
  /// use std::sync::mpsc;
  /// use lio::api::resource::Resource;
  /// use std::os::fd::FromRawFd;
  ///
  /// async fn example() -> std::io::Result<()> {
  ///     # let lio = lio::Lio::new(1024).unwrap();
  ///     # let fd0 = unsafe { Resource::from_raw_fd(0) };
  ///     # let fd1 = unsafe { Resource::from_raw_fd(1) };
  ///     let (tx, rx) = mpsc::channel();
  ///
  ///     // Send multiple operations to the same receiver
  ///     lio::api::read(&fd0, vec![0u8; 1024]).with_lio(&lio).send_with(tx.clone());
  ///     lio::api::read(&fd1, vec![0u8; 1024]).with_lio(&lio).send_with(tx.clone());
  ///
  ///     // Receive results from either operation
  ///     let (result1, buf1) = rx.recv().unwrap();
  ///     let (result2, buf2) = rx.recv().unwrap();
  ///
  ///     Ok(())
  /// }
  /// ```
  #[inline]
  pub fn send_with(self, sender: std_mpsc::Sender<T::Result>)
  where
    T::Result: Send,
  {
    self.when_done(move |res| {
      let _ = sender.send(res);
    });
  }
  /// Registers a callback to be invoked when the operation completes.
  ///
  /// # Example
  ///
  /// ```rust,no_run
  /// use std::sync::mpsc::channel;
  /// use lio::api::resource::Resource;
  /// use std::os::fd::FromRawFd;
  ///
  /// async fn example() -> std::io::Result<()> {
  ///     # let lio = lio::Lio::new(1024).unwrap();
  ///     # let fd = unsafe { Resource::from_raw_fd(0) };
  ///     let buffer = vec![0u8; 1024];
  ///     let (tx, rx) = channel();
  ///
  ///     // Use callback instead of awaiting
  ///     lio::api::read(&fd, buffer).with_lio(&lio).when_done(move |(result, buf)| {
  ///         match result {
  ///             Ok(bytes_read) => {
  ///                 println!("Read {} bytes", bytes_read);
  ///                 tx.send(buf).unwrap();
  ///             }
  ///             Err(e) => eprintln!("Error: {}", e),
  ///         }
  ///     });
  ///
  ///     // Continue with other work while I/O happens in background
  ///     // ...
  ///
  ///     // Later, wait for the result
  ///     let buffer = rx.recv().unwrap();
  ///     Ok(())
  /// }
  /// ```
  pub fn when_done<F>(self, f: F)
  where
    F: FnOnce(T::Result) + Send + 'static,
  {
    let (lio, typed_op) = self.into_lio();
    // IMPORTANT: Box the typed_op FIRST to give it a stable heap address,
    // THEN call into_op(). The Op contains pointers into the TypedOp's data,
    // so the TypedOp must be at its final heap location before into_op() is called.
    // Previously this was buggy: into_op() was called while typed_op was on the stack,
    // then typed_op was moved to the heap, leaving dangling pointers in the Op.
    let mut boxed = Box::new(typed_op);
    let op = boxed.into_op();
    lio
      .schedule(op, Registration::new_callback_boxed::<T, F>(f, boxed))
      .expect("lio error: lio should handle this");
  }
}

/// Internal handle for accessing the Lio instance.
enum LioHandle {
  /// No Lio bound - will panic if used. This is the default from `from_op()`.
  GloballyInstalled,
  /// User-provided Lio instance via `.with_lio()`.
  Custom(Lio),
}

/// A blocking receiver for operation results.
///
/// Provides blocking and non-blocking methods to receive operation results.
/// This is used for callback-based I/O completions where results are sent
/// via a channel.
pub struct Receiver<T> {
  recv: Option<std_mpsc::Receiver<T>>,
}

impl<T> Receiver<T> {
  /// Blocks the current thread until the operation completes and returns the result.
  ///
  /// This method will block indefinitely until the I/O operation finishes
  /// and the result is available.
  ///
  /// # Panics
  ///
  /// Panics if the sender is dropped without sending a result (internal error).
  pub fn recv(mut self) -> T {
    match self.get_inner() {
      Some(value) => value
        .recv()
        .expect("internal lio error: Sender dropped without sending"),
      None => unreachable!(),
    }
  }

  /// Blocks with a timeout waiting for the operation to complete.
  ///
  /// Returns `Ok(result)` if the operation completes within the timeout,
  /// or `Err(RecvTimeoutError)` if the timeout expires.
  ///
  /// # Example
  ///
  /// ```no_run
  /// use lio::{Lio, api};
  /// use std::os::fd::FromRawFd;
  /// use std::time::Duration;
  ///
  /// let mut lio = Lio::new(64).unwrap();
  /// let fd = unsafe { api::resource::Resource::from_raw_fd(1) };
  /// let mut receiver = api::write(&fd, vec![0; 10]).with_lio(&mut lio).send();
  ///
  /// // Poll with timeout
  /// match receiver.recv_timeout(Duration::from_secs(5)) {
  ///     Some(result) => println!("Got result: {:?}", result),
  ///     None => println!("Operation timed out"),
  /// }
  /// ```
  pub fn recv_timeout(&mut self, duration: Duration) -> Option<T> {
    match self.get_inner() {
      Some(value) => match value.recv_timeout(duration) {
        Ok(v) => Some(v),
        Err(err) => match err {
          std_mpsc::RecvTimeoutError::Timeout => {
            self.set_inner(value);
            None
          }
          std_mpsc::RecvTimeoutError::Disconnected => unreachable!(),
        },
      },
      None => panic!(
        "lio consumer error: Tried running BlockingReceiver::recv_timeout after first one returned value."
      ),
    }
  }

  /// Attempts to receive the result without blocking.
  ///
  /// Returns `Some(result)` if the operation has completed, or `None` if
  /// it's still in progress.
  ///
  /// Can be called multiple times. Once it returns `Some`, subsequent calls
  /// will panic.
  ///
  /// # Panics
  ///
  /// Panics if called after the first successful receive.
  ///
  /// # Example
  ///
  /// ```no_run
  /// use lio::{Lio, api};
  /// use std::os::fd::FromRawFd;
  ///
  /// let mut lio = Lio::new(64).unwrap();
  /// let fd = unsafe { api::resource::Resource::from_raw_fd(1) };
  /// let mut receiver = api::write(&fd, vec![0; 10]).with_lio(&mut lio).send();
  ///
  /// loop {
  ///     lio.try_run().unwrap(); // Process completions
  ///     if let Some(result) = receiver.try_recv() {
  ///         println!("Operation completed: {:?}", result);
  ///         break;
  ///     }
  /// }
  /// ```
  pub fn try_recv(&mut self) -> Option<T> {
    match self.get_inner() {
      Some(receiver) => match receiver.try_recv() {
        Ok(value) => Some(value),
        Err(err) => match err {
          std_mpsc::TryRecvError::Empty => {
            self.set_inner(receiver);
            None
          }
          std_mpsc::TryRecvError::Disconnected => panic!(
            "internal lio error: sender didn't send before getting dropped."
          ),
        },
      },
      None => panic!(
        "lio consumer error: Tried running BlockingReceiver::try_recv after first one returned value."
      ),
    }
  }
}

impl<T> Receiver<T> {
  fn get_inner(&mut self) -> Option<std_mpsc::Receiver<T>> {
    self.recv.take()
  }

  fn set_inner(&mut self, value: std_mpsc::Receiver<T>) {
    if self.recv.replace(value).is_some() {
      panic!("internal lio error");
    };
  }
}

impl<T> Io<T>
where
  T: Send,
{
  /// Creates a new Io from an operation.
  ///
  /// The returned Io has no Lio instance bound. You must call
  /// `.with_lio()` before consuming the operation.
  pub fn from_op(op: T) -> Self {
    Self { op, handle: LioHandle::GloballyInstalled }
  }

  /// Binds a Lio instance to this operation.
  ///
  /// This must be called before consuming the operation via `.await`, `.wait()`,
  /// `.send()`, or `.when_done()`.
  ///
  /// # Example
  ///
  /// ```no_run
  /// use lio::{Lio, api};
  /// use std::os::fd::FromRawFd;
  ///
  /// let lio = Lio::new(1024).unwrap();
  /// let fd = unsafe { api::resource::Resource::from_raw_fd(0) };
  /// let (result, buf) = api::read(&fd, vec![0u8; 1024])
  ///     .with_lio(&lio)
  ///     .wait();
  /// ```
  pub fn with_lio(self, lio: &Lio) -> Self {
    Io { op: self.op, handle: LioHandle::Custom(lio.clone()) }
  }

  fn into_lio(self) -> (Lio, T) {
    let lio = match self.handle {
      LioHandle::GloballyInstalled => lio::get_global().expect(
        "No Lio instance available. Either call install_global(lio) or use .with_lio(&lio) before consuming the operation.",
      ),
      LioHandle::Custom(lio) => lio,
    };
    (lio, self.op)
  }
}

impl<T> IntoFuture for Io<T>
where
  T: TypedOp + Unpin + 'static,
{
  type Output = T::Result;
  type IntoFuture = IoFuture<T>;

  fn into_future(self) -> Self::IntoFuture {
    let (lio, op) = self.into_lio();
    IoFuture { state: IoFutureState::Pending(op), lio }
  }
}

/// A future representing an in-progress I/O operation.
///
/// Created when a [`Io<T>`] is converted into a future via the `IntoFuture` trait,
/// typically through `.await` syntax. This type implements the `Future` trait and integrates
/// with async runtimes to provide non-blocking I/O.
///
/// # Implementation Details
///
/// - Holds the operation until first poll
/// - On first poll, submits operation to driver with waker
/// - On subsequent polls, checks for completion and updates waker
/// - Returns `Poll::Ready(result)` when the I/O operation finishes
///
/// # Usage
///
/// This type is typically not constructed directly. Instead, it's created automatically
/// when awaiting a [`Io<T>`]:
///
/// ```no_run
/// use lio::{Lio, api};
/// use std::os::fd::FromRawFd;
///
/// async fn example(lio: &Lio) {
///     let fd = unsafe { api::resource::Resource::from_raw_fd(0) };
///     let (result, buf) = api::read(&fd, vec![0; 1024]).with_lio(lio).await;
///     //                                                             ^^^^^^
///     //                                             IntoFuture creates IoFuture
/// }
/// ```
pub struct IoFuture<T> {
  state: IoFutureState<T>,
  lio: Lio,
}

enum IoFutureState<T> {
  /// Operation created but not yet submitted.
  Pending(T),
  /// Operation submitted, awaiting completion.
  /// The TypedOp is boxed to ensure it has a stable heap address
  /// before into_op() is called, so pointers in the Op remain valid.
  Inflight { id: u64, op: Box<T> },
  /// Result has been extracted. Polling again is a bug.
  Done,
}

impl<T> Future for IoFuture<T>
where
  T: TypedOp + Unpin,
{
  type Output = T::Result;

  fn poll(
    mut self: Pin<&mut Self>,
    cx: &mut Context<'_>,
  ) -> Poll<Self::Output> {
    let this = &mut *self;

    match std::mem::replace(&mut this.state, IoFutureState::Done) {
      IoFutureState::Pending(typed) => {
        // IMPORTANT: Box the typed_op FIRST to give it a stable heap address,
        // THEN call into_op(). The Op contains pointers into the TypedOp's data,
        // so the TypedOp must be at its final heap location before into_op() is called.
        let mut boxed = Box::new(typed);
        let op = boxed.into_op();
        let id = this
          .lio
          .schedule(op, Registration::new_waker(cx.waker().clone()))
          .expect("lio error: failed to schedule operation");
        this.state = IoFutureState::Inflight { id, op: boxed };
        Poll::Pending
      }

      IoFutureState::Inflight { id, op } => match this.lio.check_done(id) {
        Ok(result) => Poll::Ready(op.extract_result(result)),
        Err(crate::lio::Error::EntryNotCompleted) => {
          this.lio.set_waker(id, cx.waker().clone());
          this.state = IoFutureState::Inflight { id, op };
          Poll::Pending
        }
        Err(crate::lio::Error::EntryNotFound) => {
          panic!("lio bookkeeping bug: operation entry not found");
        }
      },

      IoFutureState::Done => {
        panic!("IoFuture polled after completion");
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::api;
  use std::task::{RawWaker, RawWakerVTable, Waker};

  #[test]
  fn test_blocking_receiver_is_send() {
    fn assert_send<T: Send>() {}

    assert_send::<Receiver<()>>();
  }

  fn noop_waker() -> Waker {
    const VTABLE: RawWakerVTable = RawWakerVTable::new(
      |p| RawWaker::new(p, &VTABLE),
      |_| {},
      |_| {},
      |_| {},
    );
    unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) }
  }

  /// Helper to run lio until completion (with timeout to avoid hanging)
  fn run_until_done(lio: &Lio) {
    use std::time::Duration;
    // Use try_run first (immediate completions), then short timeout
    lio.try_run().unwrap();
    lio.run_timeout(Duration::from_millis(10)).unwrap();
  }

  #[test]
  fn test_io_future_completes() {
    let lio = Lio::new(64).unwrap();
    let io = api::nop().with_lio(&lio);
    let mut future = io.into_future();

    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);

    // First poll submits the operation
    let poll1 = Pin::new(&mut future).poll(&mut cx);
    assert!(poll1.is_pending());

    // Run lio to complete the operation
    run_until_done(&lio);

    // Second poll should return Ready
    let poll2 = Pin::new(&mut future).poll(&mut cx);
    assert!(poll2.is_ready());
  }

  #[test]
  #[should_panic(expected = "IoFuture polled after completion")]
  fn test_io_future_panics_when_polled_after_completion() {
    let lio = Lio::new(64).unwrap();
    let io = api::nop().with_lio(&lio);
    let mut future = io.into_future();

    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);

    // Submit
    let _ = Pin::new(&mut future).poll(&mut cx);

    // Complete
    run_until_done(&lio);

    // Get result
    let poll = Pin::new(&mut future).poll(&mut cx);
    assert!(poll.is_ready());

    // This should panic
    let _ = Pin::new(&mut future).poll(&mut cx);
  }

  #[test]
  fn test_io_future_state_transitions() {
    let lio = Lio::new(64).unwrap();
    let io = api::nop().with_lio(&lio);
    let mut future = io.into_future();

    // Initial state is Pending
    assert!(matches!(future.state, IoFutureState::Pending(_)));

    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);

    // After first poll, state is Inflight
    let _ = Pin::new(&mut future).poll(&mut cx);
    assert!(matches!(future.state, IoFutureState::Inflight { .. }));

    // Complete the operation
    run_until_done(&lio);

    // After completion poll, state is Done
    let _ = Pin::new(&mut future).poll(&mut cx);
    assert!(matches!(future.state, IoFutureState::Done));
  }

  #[test]
  fn test_io_future_multiple_pending_polls() {
    let lio = Lio::new(64).unwrap();
    let io = api::nop().with_lio(&lio);
    let mut future = io.into_future();

    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);

    // First poll submits
    let poll1 = Pin::new(&mut future).poll(&mut cx);
    assert!(poll1.is_pending());

    // Polling again before completion should still be pending
    let poll2 = Pin::new(&mut future).poll(&mut cx);
    assert!(poll2.is_pending());

    // State should still be Inflight
    assert!(matches!(future.state, IoFutureState::Inflight { .. }));

    // Now complete and verify
    run_until_done(&lio);
    let poll3 = Pin::new(&mut future).poll(&mut cx);
    assert!(poll3.is_ready());
  }

  #[test]
  fn test_multiple_futures_can_coexist() {
    let lio = Lio::new(64).unwrap();

    // Create multiple futures from the same lio - this was not possible before
    let fut1 = api::nop().with_lio(&lio).into_future();
    let fut2 = api::nop().with_lio(&lio).into_future();
    let fut3 = api::nop().with_lio(&lio).into_future();

    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);

    // Pin and poll all futures
    let mut fut1 = fut1;
    let mut fut2 = fut2;
    let mut fut3 = fut3;

    let poll1 = Pin::new(&mut fut1).poll(&mut cx);
    let poll2 = Pin::new(&mut fut2).poll(&mut cx);
    let poll3 = Pin::new(&mut fut3).poll(&mut cx);

    assert!(poll1.is_pending());
    assert!(poll2.is_pending());
    assert!(poll3.is_pending());

    // Run to completion
    run_until_done(&lio);

    // All should complete
    let poll1 = Pin::new(&mut fut1).poll(&mut cx);
    let poll2 = Pin::new(&mut fut2).poll(&mut cx);
    let poll3 = Pin::new(&mut fut3).poll(&mut cx);

    assert!(poll1.is_ready());
    assert!(poll2.is_ready());
    assert!(poll3.is_ready());
  }

  #[test]
  fn test_lio_is_clone() {
    let lio1 = Lio::new(64).unwrap();
    let lio2 = lio1.clone();

    // Both refer to the same underlying instance
    let _fut1 = api::nop().with_lio(&lio1).into_future();
    let _fut2 = api::nop().with_lio(&lio2).into_future();

    // Running on either handle processes completions for both
    run_until_done(&lio1);
  }

  #[test]
  fn test_global_lio_install_and_use() {
    // Ensure no global is installed
    let _ = lio::uninstall_global();

    let lio = Lio::new(64).unwrap();
    let lio_for_run = lio.clone();
    lio::install_global(lio);

    // Now operations work without .with_lio()
    let mut receiver = api::nop().send();

    // Run the event loop to complete the operation
    run_until_done(&lio_for_run);

    let result = receiver.try_recv();
    assert!(result.is_some());
    assert!(result.unwrap().is_ok());

    // Cleanup
    let _ = lio::uninstall_global();
  }

  #[test]
  fn test_global_lio_uninstall() {
    // Ensure no global is installed
    let _ = lio::uninstall_global();

    let lio = Lio::new(64).unwrap();
    lio::install_global(lio);

    let uninstalled = lio::uninstall_global();
    assert!(uninstalled.is_some());

    // Second uninstall returns None
    let uninstalled2 = lio::uninstall_global();
    assert!(uninstalled2.is_none());
  }

  #[test]
  #[should_panic(expected = "Global Lio already installed")]
  fn test_global_lio_double_install_panics() {
    // Ensure no global is installed
    let _ = lio::uninstall_global();

    let lio1 = Lio::new(64).unwrap();
    let lio2 = Lio::new(64).unwrap();

    lio::install_global(lio1);
    lio::install_global(lio2); // Should panic
  }

  #[test]
  #[should_panic(expected = "No Lio instance available")]
  fn test_no_global_panics() {
    // Ensure no global is installed
    let _ = lio::uninstall_global();

    // This should panic because no global is installed
    api::nop().when_done(|_| {});
  }
}
