//! Operation progress tracking and result consumption.
//!
//! This module provides types for managing I/O operation lifecycles and consuming
//! their results through multiple patterns: async/await, callbacks, blocking calls,
//! and channels.
//!
//! # Overview
//!
//! The [`Progress<T>`] type is the core abstraction that wraps an I/O operation and
//! provides various methods to consume its result:
//!
//! - **Async/await**: Implements `IntoFuture`, allowing direct `.await` syntax
//! - **Blocking**: [`wait()`](Progress::wait) blocks until completion
//! - **Callbacks**: [`when_done()`](Progress::when_done) executes a closure on completion
//! - **Channels**: [`send()`](Progress::send) and [`send_with()`](Progress::send_with)
//!   deliver results via channels
//!
//! # Architecture
//!
//! ```text
//! Progress<T>
//!   ├─> IntoFuture ──> ProgressFuture<T> (async/await)
//!   ├─> wait()        (blocking)
//!   ├─> when_done(F)  (callback)
//!   ├─> send()    ──> Receiver<T>        (channel-based blocking)
//!   └─> send_with(Sender<T>)             (custom channel)
//! ```
//!
//! # Usage Patterns
//!
//! ## Async/await
//!
//! ```ignore
//! let (result, buf) = lio::read_with_buf(fd, vec![0; 1024], 0).await;
//! ```
//!
//! ## Blocking
//!
//! ```ignore
//! let (result, buf) = lio::write_with_buf(fd, vec![0; 10], 0).wait();
//! ```
//!
//! ## Callbacks
//!
//! ```ignore
//! lio::read_with_buf(fd, vec![0; 1024], 0).when_done(|(result, buf)| {
//!     println!("Read {} bytes", result.unwrap());
//! });
//! ```
//!
//! ## Channels
//!
//! ```ignore
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
//!
//! # Driver Integration
//!
//! Operations submitted through this module are processed by the lio runtime driver.
//! By default, they use the global driver instance, but can be explicitly bound to
//! a specific driver using [`with_driver()`](Progress::with_driver).

use crate::{
  backends::{SubmitErr, Submitter},
  operation::OperationExt,
  registration::StoredOp,
  worker::Worker,
};

use std::{
  future::Future,
  pin::Pin,
  sync::mpsc as std_mpsc,
  task::{Context, Poll},
  time::Duration,
};

use crate::Driver;

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
/// `Progress<T>` is the primary interface for consuming I/O operation results in lio.
/// It wraps an operation of type `T` and provides methods to retrieve the result through
/// various patterns suited to different programming models.
///
/// # Examples
/// See examples in each methods docs.
///
/// # Driver Binding
///
/// Operations use the global driver by default but can be bound to a specific
/// driver instance:
///
/// ```ignore
/// let progress = lio::read_with_buf(fd, vec![0; 1024], 0)
///     .with_driver(custom_driver);
/// ```
pub struct Progress<'a, T>
where
  T: Send,
{
  op: T,
  handle: LioHandle<'a>,
}

enum LioHandle<'a> {
  Submitter(&'a mut Submitter),
  Driver(&'a Worker),
  GlobalDriver,
}

impl<'a> LioHandle<'a> {
  pub fn submit(&mut self, op: StoredOp) -> Result<u64, SubmitErr> {
    match self {
      Self::Submitter(submitter) => submitter.submit(op),
      Self::GlobalDriver => Driver::get().submit(op),
      Self::Driver(driver) => driver.submit(op),
    }
  }
}

impl<'a, T> Progress<'a, T>
where
  T: Send,
{
  pub(crate) fn from_op(op: T) -> Self {
    Self { op, handle: LioHandle::GlobalDriver }
  }
}

impl<'a, T> Progress<'a, T>
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
  /// Returns a [`Receiver`](crate::operation::Receiver) which receives the operation result when complete.
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
  pub fn when_done<F>(mut self, f: F)
  where
    F: FnOnce(T::Result) + Send,
  {
    // let driver = self.get_driver();
    let stored = StoredOp::new_callback(Box::new(self.op), f);
    self.handle.submit(stored).expect("lio error: lio should handle this");
  }

  // /// Binds this operation to a specific driver instance.
  // ///
  // /// By default, operations use the global driver obtained via [`Driver::get()`].
  // /// This method allows you to explicitly specify which driver should process the operation,
  // /// which is useful when managing multiple driver instances for workload isolation or
  // /// resource partitioning.
  // ///
  // /// # Arguments
  // ///
  // /// * `driver` - A static reference to the driver that should process this operation
  // ///
  // /// # Examples
  // ///
  // /// ```ignore
  // /// // Create a custom driver for high-priority operations
  // /// static HIGH_PRIORITY_DRIVER: Driver = Driver::new();
  // ///
  // /// // Bind the operation to the custom driver
  // /// let progress = lio::read_with_buf(fd, vec![0; 1024], 0)
  // ///     .with_driver(&HIGH_PRIORITY_DRIVER);
  // ///
  // /// let (result, buf) = progress.await;
  // /// ```
  // ///
  // /// # Use Cases
  // ///
  // /// - **Workload isolation**: Separate critical I/O from background tasks
  // /// - **Resource partitioning**: Dedicate driver instances to specific workloads
  // /// - **Testing**: Use controlled driver instances in unit tests
  // pub fn with_driver(mut self, driver: &'static Worker) -> Self {
  //   self.handle = LioHandle::Driver(driver);
  //   self
  // }
  //
  // pub fn with_submitter(
  //   mut self,
  //   submitter: &'a mut Submitter,
  // ) -> Progress<'a, T> {
  //   self.handle = LioHandle::Submitter(submitter);
  //   self
  // }
}

impl<'a, T> IntoFuture for Progress<'a, T>
where
  T: OperationExt + Unpin + 'static,
{
  type Output = T::Result;
  type IntoFuture = ProgressFuture<'a, T>;

  fn into_future(self) -> Self::IntoFuture {
    ProgressFuture {
      status: PFStatus::NeverPolled(Some(self.op)),
      driver: self.handle,
    }
  }
}

/// A future representing an in-progress I/O operation.
///
/// Created when a [`Progress<T>`] is converted into a future via the `IntoFuture` trait,
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
/// when awaiting a `Progress<T>`:
///
/// ```ignore
/// let (result, buf) = lio::read_with_buf(fd, vec![0; 1024], 0).await;
/// //                                                          ^^^^^^
/// //                                          IntoFuture creates ProgressFuture
/// ```
pub struct ProgressFuture<'a, T> {
  status: PFStatus<T>,
  driver: LioHandle<'a>,
}

enum PFStatus<T> {
  HasPolled(u64),
  NeverPolled(Option<T>),
}

impl<'a, T> ProgressFuture<'a, T> {
  fn get_driver(&self) -> &'static Driver {
    match self.driver {
      LioHandle::Driver(_driver) => unimplemented!(),
      LioHandle::GlobalDriver => Driver::get(),
      _ => unimplemented!(),
    }
  }
}

impl<'a, T> Future for ProgressFuture<'a, T>
where
  T: OperationExt + Unpin + 'static,
{
  type Output = T::Result;

  fn poll(
    mut self: Pin<&mut Self>,
    cx: &mut Context<'_>,
  ) -> Poll<Self::Output> {
    let driver = self.get_driver();

    match self.status {
      PFStatus::HasPolled(id) => {
        match driver.check_done::<T>(id) {
          Some(result) => Poll::Ready(result),
          None => {
            // Update waker in case it changed
            driver.set_waker(id, cx.waker().clone());
            Poll::Pending
          }
        }
      }
      PFStatus::NeverPolled(ref mut op) => {
        assert!(op.is_some());
        let stored =
          StoredOp::new_waker(op.take().unwrap(), cx.waker().clone());
        let id =
          driver.submit(stored).expect("lio error: lio should handle this");
        self.status = PFStatus::HasPolled(id);
        return Poll::Pending;
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_operation_progress_is_send() {
    fn assert_send<T: Send>() {}

    // Test with a mock operation type
    // We use () as a placeholder since we just need to verify the trait bound
    assert_send::<Progress<()>>();
  }

  #[test]
  fn test_blocking_receiver_is_send() {
    fn assert_send<T: Send>() {}

    assert_send::<Receiver<()>>();
  }
}
