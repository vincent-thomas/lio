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
//! ```ignore
//!
//! // Async/await
//! let (result, buf) = lio::read_with_buf(fd, vec![0; 1024], 0).await;
//!
//! // Blocking
//! let (result, buf) = lio::write_with_buf(fd, vec![0; 10], 0).wait();
//!
//! // Callbacks
//! lio::read_with_buf(fd, vec![0; 1024], 0).when_done(|(result, buf)| {
//!     println!("Read {} bytes", result.unwrap());
//! });
//!
//! // Channels
//!
//! let receiver = lio::write_with_buf(fd, vec![0; 10], 0).send();
//! // Do other work...
//! let (result, buf) = receiver.recv();
//! ```
//!
//! # Thread Safety
//!
//! All types in this module are `Send`, allowing results to be consumed on different
//! threads than where operations were initiated. This is particularly useful for
//! delegating I/O completion handling to dedicated threads.

use crate::{lio::Lio, operation::OperationExt, registration::StoredOp};

use std::{
  future::Future,
  pin::Pin,
  sync::mpsc as std_mpsc,
  task::{Context, Poll},
  time::Duration,
};

/// Internal handle for accessing the Lio instance.
enum LioHandle<'a> {
  /// No Lio bound - will panic if used. This is the default from `from_op()`.
  GloballyInstalled,
  /// User-provided Lio instance via `.with_lio()`.
  Custom(&'a mut Lio),
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
  /// ```ignore
  /// use std::time::Duration;
  ///
  /// match receiver.recv_timeout(Duration::from_secs(5)) {
  ///     Ok(result) => println!("Got result: {:?}", result),
  ///     Err(_) => println!("Operation timed out"),
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
  /// ```ignore
  /// loop {
  ///     if let Some(result) = receiver.try_recv() {
  ///         println!("Operation completed: {:?}", result);
  ///         break;
  ///     }
  ///     // Do other work...
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
/// ```ignore
/// let mut lio = Lio::new_with_backend(backend, 1024)?;
/// let (result, buf) = lio::read(fd, buffer)
///     .with_lio(&mut lio)
///     .await;
/// ```
///
/// # Examples
/// See examples in each method's docs.
#[must_use = "Io doesn't schedule any operation on itself."]
pub struct Io<'a, T>
where
  T: Send,
{
  op: T,
  handle: LioHandle<'a>,
}

impl<T> Io<'static, T>
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
}

impl<'a, T> Io<'a, T>
where
  T: Send,
{
  /// Binds a Lio instance to this operation.
  ///
  /// This must be called before consuming the operation via `.await`, `.wait()`,
  /// `.send()`, or `.when_done()`.
  ///
  /// # Example
  ///
  /// ```ignore
  /// let mut lio = Lio::new_with_backend(backend, 1024)?;
  /// let (result, buf) = lio::read(fd, buffer)
  ///     .with_lio(&mut lio)
  ///     .await;
  /// ```
  pub fn with_lio<'b>(self, lio: &'b mut Lio) -> Io<'b, T> {
    Io { op: self.op, handle: LioHandle::Custom(lio) }
  }

  fn into_lio(self) -> (&'a mut Lio, T) {
    let lio = match self.handle {
      LioHandle::GloballyInstalled => {
        panic!(
          "No Lio instance bound. Call .with_lio(&mut lio) before consuming the operation."
        )
      }
      LioHandle::Custom(lio) => lio,
    };
    (lio, self.op)
  }
}

impl<'a, T> Io<'a, T>
where
  T: OperationExt + 'static,
{
  /// Block the current thread until the operation completes and return the result.
  ///
  /// This method still makes use of lio's non-blocking core.
  ///
  /// # Example
  /// ```rust
  /// let fd = 1;
  /// lio::init();
  /// lio::start();
  /// let (result, buf) = lio::write_with_buf(fd, vec![0u8; 10], 0).blocking();
  ///
  /// match result {
  ///     Ok(_) => println!("success"),
  ///     Err(err) => eprintln!("Error: {}", err),
  /// }
  ///
  /// lio::stop();
  /// lio::exit();
  /// ```
  #[inline]
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
  /// ```ignore
  /// let fd = 1;
  /// lio::init();
  /// let buf = vec![0; 10];
  /// let receiver = lio::write(fd, buf, 0).send();
  /// lio::tick();
  /// let (result, buffer) = receiver.recv();
  /// let _ = result.unwrap();
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
  /// ```rust
  /// use std::sync::mpsc;
  ///
  /// async fn example() -> std::io::Result<()> {
  ///     lio::init();
  ///     lio::start();
  ///     let (tx, rx) = mpsc::channel();
  ///
  ///     // Send multiple operations to the same receiver
  ///     lio::read_with_buf(0, vec![0u8; 1024], 0).send_with(tx.clone());
  ///     lio::read_with_buf(1, vec![0u8; 1024], 0).send_with(tx.clone());
  ///
  ///     // Receive results from either operation
  ///     let (result1, buf1) = rx.recv().unwrap();
  ///     let (result2, buf2) = rx.recv().unwrap();
  ///
  ///     lio::stop();
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
  /// ```rust
  /// use std::sync::mpsc::channel;
  ///
  /// async fn example() -> std::io::Result<()> {
  ///     lio::init();
  ///     let fd = 0;
  ///     let buffer = vec![0u8; 1024];
  ///     let (tx, rx) = channel();
  ///
  ///     // Use callback instead of awaiting
  ///     lio::read_with_buf(fd, buffer, 0).when_done(move |(result, buf)| {
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
    let (lio, op) = self.into_lio();
    let stored = StoredOp::new_callback(Box::new(op), f);
    lio.schedule(stored).expect("lio error: lio should handle this");
  }
}

impl<'a, T> IntoFuture for Io<'a, T>
where
  T: OperationExt + Unpin + 'static,
{
  type Output = T::Result;
  type IntoFuture = IoFuture<'a, T>;

  fn into_future(self) -> Self::IntoFuture {
    let (lio, op) = self.into_lio();
    IoFuture { status: PFStatus::NeverPolled(Some(op)), lio }
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
/// ```ignore
/// let (result, buf) = lio::read_with_buf(fd, vec![0; 1024], 0).await;
/// //                                                          ^^^^^^
/// //                                          IntoFuture creates IoFuture
/// ```
pub struct IoFuture<'a, T> {
  status: PFStatus<T>,
  lio: &'a mut Lio,
}

enum PFStatus<T> {
  HasPolled(u64),
  NeverPolled(Option<T>),
}

impl<'a, T> Future for IoFuture<'a, T>
where
  T: OperationExt + Unpin + 'static,
{
  type Output = T::Result;

  fn poll(
    mut self: Pin<&mut Self>,
    cx: &mut Context<'_>,
  ) -> Poll<Self::Output> {
    // Get mutable access to self fields
    let this = &mut *self;

    match &mut this.status {
      PFStatus::HasPolled(id) => {
        match this.lio.check_done::<T>(*id) {
          Ok(result) => Poll::Ready(result),
          Err(crate::lio::Error::EntryNotCompleted) => {
            // Update waker in case it changed
            this.lio.set_waker(*id, cx.waker().clone());
            Poll::Pending
          }
          Err(crate::lio::Error::EntryNotFound) => {
            panic!("lio bookkeeping bug: operation entry not found");
          }
        }
      }
      PFStatus::NeverPolled(op) => {
        if let Some(taken_op) = op.take() {
          let stored =
            StoredOp::new_waker(Box::new(taken_op), cx.waker().clone());
          let id = this
            .lio
            .schedule(stored)
            .expect("lio error: lio should handle this");
          this.status = PFStatus::HasPolled(id);
          Poll::Pending
        } else {
          unreachable!()
        }
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  // Note: Io<'a, T> is not Send because it holds &'a mut Lio.
  // This is intentional for thread-per-core design where operations
  // are tied to their owning thread's Lio instance.

  #[test]
  fn test_blocking_receiver_is_send() {
    fn assert_send<T: Send>() {}

    assert_send::<Receiver<()>>();
  }
}
