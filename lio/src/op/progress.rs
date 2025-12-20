use crate::op::Operation;

use std::io;
use std::sync::mpsc::Receiver;
use std::time::Duration;
use std::{
  future::Future,
  pin::Pin,
  task::{Context, Poll},
};
use std::{
  mem,
  sync::mpsc::{self, TryRecvError},
};

use crate::{
  Driver,
  op::{self, DetachSafe},
};

/// A blocking receiver for operation results.
///
/// Provides blocking and non-blocking methods to receive operation results.
/// This is used for callback-based I/O completions where results are sent
/// via a channel.
pub struct BlockingReceiver<T> {
  recv: Option<Receiver<T>>,
}

/// Error returned when [`BlockingReceiver::recv_timeout`] times out.
#[derive(Debug)]
pub struct RecvTimeoutError;

impl<T> BlockingReceiver<T> {
  fn get_inner(&mut self) -> Option<Receiver<T>> {
    mem::replace(&mut self.recv, None)
  }

  fn set_inner(&mut self, value: Receiver<T>) {
    if let Some(_) = mem::replace(&mut self.recv, Some(value)) {
      panic!("internal lio error");
    };
  }

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
  pub fn recv_timeout(
    mut self,
    duration: Duration,
  ) -> Result<T, RecvTimeoutError> {
    match self.get_inner() {
      Some(value) => value.recv_timeout(duration).map_err(|_| RecvTimeoutError),
      None => unreachable!(),
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
          TryRecvError::Empty => {
            self.set_inner(receiver);
            None
          }
          TryRecvError::Disconnected => panic!(
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

/// Represents the progress of an I/O operation across different platforms.
///
/// This enum provides a unified interface for tracking I/O operations regardless
/// of the underlying platform implementation. It automatically selects the most
/// efficient execution method for each platform.
///
/// # Examples
///
/// ```rust
/// use lio::OperationProgress;
/// use std::os::fd::RawFd;
///
/// async fn example() -> std::io::Result<()> {
///     lio::init();
///     let fd: RawFd = 0; // stdin
///     let buffer = vec![0u8; 1024];
///     
///     let progress: OperationProgress<lio::op::Read> = lio::read_with_buf(fd, buffer, 0);
///     let (result_bytes_read, buf) = progress.await;
///     
///     println!("Read {} bytes", result_bytes_read?);
///
///     lio::exit();
///     Ok(())
/// }
/// ```
pub enum OperationProgress<T> {
  StoreTracked { id: u64 },
  Threaded { id: u64 },
  FromResult { res: Option<io::Result<i32>>, operation: T },
}

impl<T: op::Operation> OperationProgress<T> {
  /// Detaches this progress tracker from the driver without binding it to any object.
  ///
  /// This function is useful when you want to clean up the operation registration
  /// without waiting for the operation to complete. It's automatically called
  /// when the `OperationProgress` is dropped.
  ///
  /// # Examples
  ///
  /// ```rust
  /// use lio::OperationProgress;
  /// use std::os::fd::RawFd;
  ///
  /// async fn detach_example() -> std::io::Result<()> {
  ///     lio::init();
  ///
  ///     let fd: RawFd = 0;
  ///     let buffer = vec![0u8; 1024];
  ///     let progress: OperationProgress<lio::op::Read> = lio::read_with_buf(fd, buffer, 0);
  ///     
  ///     // Detach without waiting for completion
  ///     progress.detach();
  ///
  ///     lio::exit();
  ///     
  ///     Ok(())
  /// }
  /// ```
  pub fn detach(self)
  where
    T: DetachSafe + Send + 'static,
  {
    self.when_done(drop);
  }

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
  pub fn blocking(self) -> T::Result
  where
    T::Result: Send,
    T: Send + 'static,
  {
    self.send().recv()
  }

  /// Convert the operation into a channel receiver.
  ///
  /// Returns a [`BlockingReceiver`] which receives the operation result when complete.
  /// Useful for integrating with channel-based async code or when you need to wait
  /// for the result in a different context than where the operation was started.
  ///
  /// # Example
  /// ```ignore
  /// let fd = 1;
  /// lio::init();
  /// let buf = vec![0; 10];
  /// let receiver = lio::write_with_buf(fd, buf, 0).send();
  /// lio::tick();
  /// let (result, buffer) = receiver.recv();
  /// let _ = result.unwrap();
  /// ```
  pub fn send(self) -> BlockingReceiver<T::Result>
  where
    T::Result: Send,
    T: Send + 'static,
  {
    let (sender, receiver) = mpsc::channel();

    self.send_with(sender);

    BlockingReceiver { recv: Some(receiver) }
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
  pub fn send_with(self, sender: mpsc::Sender<T::Result>)
  where
    T::Result: Send,
    T: Send + 'static,
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
  pub fn when_done<F>(mut self, callback: F)
  where
    F: FnOnce(T::Result) + Send + 'static,
    T: Send + 'static,
  {
    match self {
      OperationProgress::StoreTracked { id } => {
        Driver::get().set_callback::<T, F>(id, callback);
        std::mem::forget(self); // Prevent Drop from cancelling the operation
      }
      OperationProgress::Threaded { id } => {
        Driver::get().set_callback::<T, F>(id, callback);
        std::mem::forget(self); // Prevent Drop from cancelling the operation
      }
      OperationProgress::FromResult { ref mut res, ref mut operation } => {
        let res = res.take().expect("Blocking operation already consumed");
        let output = operation.result(res);
        callback(output);
      }
    }
  }
}

impl<T> OperationProgress<T>
where
  T: op::Operation,
{
  pub(crate) fn new_store_tracked(id: u64) -> Self
  where
    T: Operation,
  {
    Self::StoreTracked { id }
  }

  pub(crate) fn new_from_result(operation: T, result: io::Result<i32>) -> Self {
    Self::FromResult { operation, res: Some(result) }
  }
  pub(crate) fn new_threaded(id: u64) -> Self {
    Self::Threaded { id }
  }
}

impl<T> IntoFuture for OperationProgress<T>
where
  T: Operation + Unpin,
{
  type Output = T::Result;
  type IntoFuture = FutRecv<T>;

  fn into_future(self) -> Self::IntoFuture {
    match self {
      OperationProgress::StoreTracked { id } => FutRecv::StoreTracked { id },
      OperationProgress::Threaded { id } => FutRecv::Threaded { id },
      OperationProgress::FromResult { res, operation } => {
        FutRecv::FromResult { res, operation }
      }
    }
  }
}

pub enum FutRecv<T> {
  StoreTracked { id: u64 },
  Threaded { id: u64 },
  FromResult { res: Option<io::Result<i32>>, operation: T },
}

impl<T> Future for FutRecv<T>
where
  T: Operation + Unpin,
{
  type Output = T::Result;

  fn poll(
    mut self: Pin<&mut Self>,
    cx: &mut Context<'_>,
  ) -> Poll<Self::Output> {
    fn check_done<T>(id: u64, cx: &mut Context<'_>) -> Poll<T::Result>
    where
      T: op::Operation,
    {
      match Driver::get().check_done::<T>(id) {
        Some(result) => Poll::Ready(result),
        None => {
          Driver::get().set_waker(id, cx.waker().clone());
          Poll::Pending
        }
      }
    }

    match *self {
      FutRecv::StoreTracked { id } => check_done::<T>(id, cx),
      FutRecv::Threaded { id } => check_done::<T>(id, cx),
      FutRecv::FromResult { ref mut res, ref mut operation } => {
        let result = operation.result(res.take().expect("Already awaited."));
        Poll::Ready(result)
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
    assert_send::<OperationProgress<()>>();
  }

  #[test]
  fn test_blocking_receiver_is_send() {
    fn assert_send<T: Send>() {}

    assert_send::<BlockingReceiver<()>>();
  }
}
