use crate::op::OperationExt;
use crate::registration::StoredOp;

use std::marker::PhantomData;
use std::mem;
use std::sync::mpsc as std_mpsc;
use std::time::Duration;
use std::{
  future::Future,
  pin::Pin,
  task::{Context, Poll},
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

/// Error returned when [`BlockingReceiver::recv_timeout`] times out.
#[derive(Debug)]
pub struct RecvTimeoutError;

impl<T> Receiver<T> {
  fn get_inner(&mut self) -> Option<std_mpsc::Receiver<T>> {
    mem::replace(&mut self.recv, None)
  }

  fn set_inner(&mut self, value: std_mpsc::Receiver<T>) {
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

pub struct Progress<T> {
  op: T,
}

impl<T> Progress<T> {
  pub(crate) fn from_op(op: T) -> Self {
    Self { op }
  }
}

impl<T> Progress<T> {
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
    T: OperationExt + 'static,
    T::Result: Send,
    T: Send + 'static,
  {
    self.send().recv()
  }
  /// Convert the operation into a channel receiver.
  ///
  /// Returns a [`Receiver`](crate::op::Receiver) which receives the operation result when complete.
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
  pub fn send(self) -> Receiver<T::Result>
  where
    T: OperationExt + Send + 'static,
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
  pub fn send_with(self, sender: std_mpsc::Sender<T::Result>)
  where
    T: OperationExt + Send + 'static,
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
    T: OperationExt + Send + 'static,
    F: FnOnce(T::Result) + Send,
  {
    let stored = StoredOp::new_callback(Box::new(self.op), f);
    Driver::submit(stored).expect("lio error: lio should handle this");
  }
}

impl<T> IntoFuture for Progress<T>
where
  T: OperationExt + Unpin + 'static,
{
  type Output = T::Result;
  type IntoFuture = FutRecv<T>;

  fn into_future(self) -> Self::IntoFuture {
    let stored = StoredOp::new_waker(self.op);
    let id = Driver::submit(stored).expect("lio error: lio should handle this");
    FutRecv { id, _m: PhantomData }
  }
}

pub struct FutRecv<T> {
  id: u64,
  _m: PhantomData<T>,
}

impl<T> Future for FutRecv<T>
where
  T: OperationExt + Unpin + 'static,
{
  type Output = T::Result;

  fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
    match Driver::get().check_done::<T>(self.id) {
      Some(result) => Poll::Ready(result),
      None => {
        Driver::get().set_waker(self.id, cx.waker().clone());
        Poll::Pending
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
