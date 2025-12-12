use crate::op::Operation;

use std::io;
use std::sync::mpsc::Receiver;
#[cfg(feature = "high")]
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

pub struct BlockingReceiver<T> {
  recv: Option<Receiver<T>>,
}

impl<T> BlockingReceiver<T> {
  fn get_inner(&mut self) -> Option<Receiver<T>> {
    mem::replace(&mut self.recv, None)
  }

  fn set_inner(&mut self, value: Receiver<T>) {
    if let Some(_) = mem::replace(&mut self.recv, Some(value)) {
      panic!("internal lio error");
    };
  }
  pub fn recv(mut self) -> T {
    match self.get_inner() {
      Some(value) => value
        .recv()
        .expect("internal lio error: Sender dropped without sending"),
      None => unreachable!(),
    }
  }

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
/// # Platform-Specific Behavior
///
/// - **Linux**: Uses io_uring for maximum performance when supported
/// - **Other platforms**: Falls back to polling-based async I/O or blocking execution
///
/// # Examples
///
/// ```rust
/// use lio::{read, OperationProgress};
/// use std::os::fd::RawFd;
///
/// async fn example() -> std::io::Result<()> {
///     lio::init();
///     let fd: RawFd = 0; // stdin
///     let buffer = vec![0u8; 1024];
///     
///     let progress: OperationProgress<lio::op::Read> = read(fd, buffer, 0);
///     let (result_bytes_read, buf) = progress.await;
///     
///     println!("Read {} bytes", result_bytes_read?);
///     Ok(())
/// }
/// ```
pub enum OperationProgress<T> {
  StoreTracked { id: u64 },
  Threaded { id: u64 },
  FromResult { res: Option<io::Result<i32>>, operation: T },
}

unsafe impl<T> Send for OperationProgress<T> where T: Send {}

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
  /// use lio::{read, OperationProgress};
  /// use std::os::fd::RawFd;
  ///
  /// async fn detach_example() -> std::io::Result<()> {
  ///     let fd: RawFd = 0;
  ///     let buffer = vec![0u8; 1024];
  ///     let progress: OperationProgress<lio::op::Read> = read(fd, buffer, 0);
  ///     
  ///     // Detach without waiting for completion
  ///     progress.detach();
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
  /// A easy way of calling non-async syscalls This method still makes use of
  /// lio's non-blocking core.
  ///
  /// # Example
  /// ```rust
  /// # let fd = 0;
  /// lio::init();
  /// let receiver = lio::write(fd, vec![0u8; 10], 0).send();
  /// lio::tick();
  ///
  /// let (result, buf) = receiver.recv();
  /// match result {
  ///     Ok(_) => println!("success"),
  ///     Err(err) => eprintln!("Error: {}", err),
  /// }
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
  /// Returns a oneshot receiver that will receive the operation result when complete.
  /// Useful for integrating with channel-based async code or when you need to wait
  /// for the result in a different context than where the operation was started.
  ///
  /// # Example
  /// ```ignore
  /// let fd = 1;
  /// // Some fd defined.
  /// // ...
  /// lio::init();
  /// let buf = vec![0; 10];
  /// let receiver = lio::write(fd, buf, 0).send();
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
  /// This is a variant of [`send()`](Self::send) that allows you to provide your own
  /// `Sender` instead of creating a new channel. Useful when integrating with existing
  /// channel-based architectures or when you want to multiplex multiple operation results
  /// into a single receiver.
  ///
  /// # Arguments
  ///
  /// * `sender` - The channel sender that will receive the operation result
  ///
  /// # Examples
  ///
  /// ```rust
  /// use lio::read;
  /// use std::sync::mpsc;
  ///
  /// async fn example() -> std::io::Result<()> {
  ///     lio::init();
  ///     let (tx, rx) = mpsc::channel();
  ///
  ///     // Send multiple operations to the same receiver
  ///     read(0, vec![0u8; 1024], 0).send_with(tx.clone());
  ///     read(1, vec![0u8; 1024], 0).send_with(tx.clone());
  ///
  ///     // Receive results from either operation
  ///     let (result1, buf1) = rx.recv().unwrap();
  ///     let (result2, buf2) = rx.recv().unwrap();
  ///
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
  /// This method takes ownership of the `OperationProgress`, preventing it from being
  /// polled as a Future. The callback will receive the operation result when the I/O
  /// operation completes.
  ///
  /// # Mutual Exclusion with Future Polling
  ///
  /// Once `when_done` is called, the operation cannot be polled as a Future. This is
  /// enforced by taking ownership of `self`. You must choose one execution model:
  /// - **Await the Future**: Use `.await` to get the result synchronously in your async code
  /// - **Use a callback**: Use `.when_done()` for fire-and-forget operations or when you
  ///   need the result in a different context
  ///
  /// # Callback Requirements
  ///
  /// The callback must be `FnOnce(T::Result) + Send + 'static`:
  /// - `FnOnce`: The callback is invoked exactly once when the operation completes
  /// - `Send`: The callback may be executed on a background I/O thread
  /// - `'static`: The callback must not borrow data with lifetimes (use `move` closures
  ///   with owned data or `Arc`/`Arc<Mutex<T>>` for shared state)
  ///
  /// # Platform-Specific Behavior
  ///
  /// - **Blocking operations**: The callback is invoked immediately (synchronously)
  /// - **Async operations** (io_uring/polling): The callback is invoked asynchronously
  ///   on the background I/O thread when the operation completes
  ///
  /// # Examples
  ///
  /// ## Basic callback usage
  ///
  /// ```rust
  /// use lio::read;
  /// use std::sync::mpsc::channel;
  ///
  /// async fn example() -> std::io::Result<()> {
  ///     lio::init();
  ///     # let fd = 0;
  ///     let buffer = vec![0u8; 1024];
  ///     let (tx, rx) = channel();
  ///
  ///     // Use callback instead of awaiting
  ///     read(fd, buffer, 0).when_done(move |(result, buf)| {
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
  ///
  /// ## Shared state with Arc
  ///
  /// ```rust
  /// use lio::write;
  /// use std::sync::{Arc, Mutex};
  ///
  /// async fn example() -> std::io::Result<()> {
  ///     lio::init();
  ///     # let fd = 1;
  ///     let data = b"Hello, callbacks!".to_vec();
  ///     let result = Arc::new(Mutex::new(None));
  ///     let result_clone = result.clone();
  ///
  ///     write(fd, data, 0).when_done(move |(bytes_written, _buf)| {
  ///         *result_clone.lock().unwrap() = Some(bytes_written);
  ///     });
  ///
  ///     // Continue with other work...
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

// Threading backend works on all platforms
impl<T> OperationProgress<T> where T: op::Operation {}

/// Implements `Future` for polling-based operations on non-Linux platforms.
///
/// This implementation handles operations that use polling-based async I/O,
/// automatically re-registering for events when operations would block.
#[cfg(feature = "high")]
impl<T> Future for OperationProgress<T>
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
      OperationProgress::StoreTracked { id }
      | OperationProgress::Threaded { id } => check_done::<T>(id, cx),
      OperationProgress::FromResult { ref mut res, ref mut operation } => {
        let result = operation.result(res.take().expect("Already awaited."));
        Poll::Ready(result)
      }
    }
  }
}

impl<T> Drop for OperationProgress<T> {
  fn drop(&mut self) {
    match self {
      OperationProgress::StoreTracked { id: _ } => {
        // Driver::get().detach(*id);
      }
      OperationProgress::Threaded { id: _ } => {}
      OperationProgress::FromResult { res: _, operation: _ } => {}
    }
  }
}
