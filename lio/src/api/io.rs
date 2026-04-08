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
//!
//! let mut lio = Lio::new(64).unwrap();
//! let fd = api::resource::Resource::stdout();
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

use crate::{api::op::OpModel, lio, lio::Lio, registration::Registration};

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
  T: OpModel,
{
  // /// Block the current thread until the operation completes and return the result.
  // ///
  // /// # Deprecated
  // ///
  // /// This method is deprecated because it requires someone else to run the event loop
  // /// (e.g., a background thread calling `lio.run()`). Use `.send()` with manual event
  // /// loop management, or `.when_done()` for callback-based completion instead.
  // #[inline]
  // #[deprecated(
  //   since = "0.4.0",
  //   note = "wait() requires external event loop management. Use .send() with manual lio.run() calls or .when_done() instead."
  // )]
  // pub fn wait(self) -> T::Result
  // where
  //   T::Item: Send,
  // {
  //   self.send().recv()
  // }
  /// Convert the operation into a channel receiver.
  ///
  /// Returns a [`Receiver`] which receives the operation result when complete.
  /// Useful for integrating with channel-based async code or when you need to wait
  /// for the result in a different context than where the operation was started.
  ///
  /// # Example
  /// ```no_run
  /// use lio::{Lio, api};
  ///
  /// let lio = Lio::new(64).unwrap();
  /// let fd = api::resource::Resource::stdout();
  /// let receiver = api::write(&fd, vec![0; 10]).with_lio(&lio).send();
  /// lio.try_run().unwrap();
  /// let (result, buffer) = receiver.recv();
  /// ```
  #[inline]
  pub fn send(self) -> Receiver<T::Item>
  where
    T::Item: Send,
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
  ///
  /// async fn example() -> std::io::Result<()> {
  ///     # let lio = lio::Lio::new(1024).unwrap();
  ///     # let fd0 = Resource::stdin();
  ///     # let fd1 = Resource::stdout();
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
  pub fn send_with(self, sender: std_mpsc::Sender<T::Item>)
  where
    T::Item: Send,
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
  ///
  /// async fn example() -> std::io::Result<()> {
  ///     # let lio = lio::Lio::new(1024).unwrap();
  ///     # let fd = Resource::stdin();
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
    F: Fn(T::Item) + Send + 'static,
  {
    let (lio, stream_op) = self.into_lio();

    // Box the StreamOp to give it a stable heap address before creating Registration.
    // The Registration stores the StreamOp and calls send_op()/result() on it.
    let boxed = Box::new(stream_op);
    let registration = Registration::new_callback::<T, F>(f, boxed);

    lio.schedule(registration).expect("lio error: lio should handle this");
  }
}

/// Internal handle for accessing the Lio instance.
#[derive(Clone)]
enum LioHandle {
  /// No Lio bound - will panic if used. This is the default from `from_op()`.
  GloballyInstalled,
  /// User-provided Lio instance via `.with_lio()`.
  Custom(Lio),
}

impl LioHandle {
  fn into_lio(self) -> Lio {
    match self {
      LioHandle::GloballyInstalled => lio::get_global().expect(
        "No Lio instance available. Either call install_global(lio) or use .with_lio(&lio) before consuming the operation.",
      ),
      LioHandle::Custom(lio) => lio,
    }
  }

  fn lio(&self) -> Lio {
    self.clone().into_lio()
  }
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
  /// use std::time::Duration;
  ///
  /// let mut lio = Lio::new(64).unwrap();
  /// let fd = api::resource::Resource::stdout();
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
  ///
  /// let mut lio = Lio::new(64).unwrap();
  /// let fd = api::resource::Resource::stdout();
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
  ///
  /// let lio = Lio::new(1024).unwrap();
  /// let fd = api::resource::Resource::stdin();
  /// let read_result_recv = api::read(&fd, vec![0u8; 1024])
  ///     .with_lio(&lio)
  ///     .send();
  /// ```
  pub fn with_lio(self, lio: &Lio) -> Self {
    Io { op: self.op, handle: LioHandle::Custom(lio.clone()) }
  }

  fn into_lio(self) -> (Lio, T) {
    let lio = self.handle.into_lio();
    (lio, self.op)
  }

  /// Extracts the inner operation from this Io.
  ///
  /// This is useful for wrapping operations with combinators like `timeout`.
  pub fn into_inner(self) -> T {
    self.op
  }
}

impl<T> IntoFuture for Io<T>
where
  T: OpModel + Unpin + 'static,
{
  type Output = T::Item;
  type IntoFuture = IoStreamFuture<T>;

  fn into_future(self) -> Self::IntoFuture {
    let (lio, stream_op) = self.into_lio();
    IoStreamFuture { state: IoStreamFutureState::Pending(stream_op), lio }
  }
}

/// A future representing a single-shot StreamOp operation.
///
/// Owns the channel receiver to get typed results.
pub struct IoStreamFuture<T>
where
  T: OpModel,
{
  state: IoStreamFutureState<T>,
  lio: Lio,
}

enum IoStreamFutureState<T>
where
  T: OpModel,
{
  /// Not yet submitted.
  Pending(T),
  /// Submitted and awaiting completion.
  Inflight { id: u64, receiver: std::sync::mpsc::Receiver<T::Item> },
  /// Done.
  Done,
}

impl<T> Future for IoStreamFuture<T>
where
  T: OpModel + Unpin,
{
  type Output = T::Item;

  fn poll(
    mut self: Pin<&mut Self>,
    cx: &mut Context<'_>,
  ) -> Poll<Self::Output> {
    let this = &mut *self;

    match std::mem::replace(&mut this.state, IoStreamFutureState::Done) {
      IoStreamFutureState::Pending(stream_op) => {
        // First poll - create channel and schedule operation
        let (tx, rx) = std::sync::mpsc::channel();

        let boxed = Box::new(stream_op);
        let registration =
          Registration::new_waker(cx.waker().clone(), tx, boxed);

        let id = this
          .lio
          .schedule(registration)
          .expect("lio error: failed to schedule operation");

        this.state = IoStreamFutureState::Inflight { id, receiver: rx };
        Poll::Pending
      }

      IoStreamFutureState::Inflight { id, receiver } => {
        // Try to receive typed result from channel
        match receiver.try_recv() {
          Ok(item) => {
            // Got the result!
            Poll::Ready(item)
          }
          Err(std::sync::mpsc::TryRecvError::Empty) => {
            // No result yet, update waker and stay in Inflight
            this.lio.set_waker(id, cx.waker().clone());
            this.state = IoStreamFutureState::Inflight { id, receiver };
            Poll::Pending
          }
          Err(std::sync::mpsc::TryRecvError::Disconnected) => {
            panic!("Channel disconnected - sender dropped prematurely");
          }
        }
      }

      IoStreamFutureState::Done => {
        panic!("IoStreamFuture polled after completion");
      }
    }
  }
}

// ============================================================================
// IoStream - Async Stream for multi-item operations
// ============================================================================

/// An async stream that yields multiple items from an operation.
///
/// Unlike [`Io`] which produces a single result, `IoStream` can yield
/// multiple items over time. This is useful for operations like accepting
/// connections from a listening socket or watching for file changes.
///
/// # Async Iterator Pattern
///
/// `IoStream` provides an async iterator interface via the `next()` method:
///
/// ```ignore
/// use lio::{Lio, api};
///
/// async fn accept_connections(lio: &Lio) -> std::io::Result<()> {
///     # use lio::api::resource::Resource;
///     # let listener = Resource::stdin(); // placeholder
///     let mut stream = api::accept_stream(&listener).with_lio(lio);
///
///     while let Some(result) = stream.next().await {
///         let conn = result?;
///         println!("Accepted connection from {}", conn.peer_addr()?);
///         // Handle client...
///     }
///     Ok(())
/// }
/// ```
///
/// # Platform Differences
///
/// - **io_uring (Linux)**: Uses multishot operations where possible. A single
///   submission can yield multiple completions without resubmission.
/// - **kqueue/epoll**: Each completion triggers automatic resubmission of the
///   operation to continue the stream.
pub struct IoStream<T>
where
  T: OpModel,
{
  state: IoStreamState<T>,
  handle: LioHandle,
}

enum IoStreamState<T>
where
  T: OpModel,
{
  /// Not yet submitted - waiting for first `next()` call.
  Pending(T),
  /// Submitted and awaiting completion.
  Inflight { id: u64, receiver: std::sync::mpsc::Receiver<T::Item> },
  /// Stream is done, no more items.
  Done,
}

impl<T> IoStream<T>
where
  T: OpModel + Unpin,
{
  /// Creates a new `IoStream` from a streaming operation.
  pub fn from_op(op: T) -> Self {
    Self {
      state: IoStreamState::Pending(op),
      handle: LioHandle::GloballyInstalled,
    }
  }

  /// Binds a Lio instance to this stream.
  pub fn with_lio(mut self, lio: &Lio) -> Self {
    self.handle = LioHandle::Custom(lio.clone());
    self
  }

  /// Returns the next item from the stream.
  ///
  /// Returns `Some(item)` if the stream has more items, or `None` if the
  /// stream is exhausted.
  ///
  /// # Example
  ///
  /// ```ignore
  /// # use lio::{Lio, api};
  /// # use lio::api::resource::Resource;
  /// # async fn example(lio: &Lio, listener: &Resource) -> std::io::Result<()> {
  /// let mut stream = api::accept_stream(listener).with_lio(lio);
  /// while let Some(result) = stream.next().await {
  ///     let conn = result?;
  ///     println!("New connection from {}", conn.peer_addr()?);
  /// }
  /// # Ok(())
  /// # }
  /// ```
  pub async fn next(&mut self) -> Option<T::Item> {
    IoStreamNextFuture { stream: self }.await
  }

  /// Convert the stream into a channel receiver.
  pub fn send(self) -> StreamReceiver<T::Item>
  where
    T::Item: Send,
  {
    let (sender, receiver) = std_mpsc::channel();
    self.send_with(sender);
    StreamReceiver { recv: receiver }
  }

  /// Sends each stream item through a provided channel sender.
  pub fn send_with(mut self, sender: std_mpsc::Sender<T::Item>)
  where
    T::Item: Send,
  {
    let lio = self.handle.lio();
    let op = match std::mem::replace(&mut self.state, IoStreamState::Done) {
      IoStreamState::Pending(op) => op,
      IoStreamState::Inflight { .. } => {
        panic!("lio consumer error: stream already started before send_with()")
      }
      IoStreamState::Done => {
        panic!(
          "lio consumer error: stream already completed before send_with()"
        )
      }
    };

    let boxed = Box::new(op);
    let reg = Registration::new_callback(
      move |item| {
        let _ = sender.send(item);
      },
      boxed,
    );

    lio.schedule(reg).expect("lio error: failed to schedule stream operation");
  }
}

/// Future for a single `next()` call on an IoStream.
struct IoStreamNextFuture<'a, T: OpModel> {
  stream: &'a mut IoStream<T>,
}

impl<T: OpModel + Unpin> Future for IoStreamNextFuture<'_, T> {
  type Output = Option<T::Item>;

  fn poll(
    mut self: Pin<&mut Self>,
    cx: &mut Context<'_>,
  ) -> Poll<Self::Output> {
    let this = &mut *self;
    let stream = &mut *this.stream;

    match std::mem::replace(&mut stream.state, IoStreamState::Done) {
      IoStreamState::Pending(stream_op) => {
        // First poll - create channel and schedule operation
        let (tx, rx) = std::sync::mpsc::channel();

        let boxed = Box::new(stream_op);
        let registration =
          Registration::new_waker(cx.waker().clone(), tx, boxed);

        match stream.handle.lio().schedule(registration) {
          Ok(id) => {
            stream.state = IoStreamState::Inflight { id, receiver: rx };
            Poll::Pending
          }
          Err(_) => {
            stream.state = IoStreamState::Done;
            Poll::Ready(None)
          }
        }
      }

      IoStreamState::Inflight { id, receiver } => {
        // Try to receive typed result from channel
        match receiver.try_recv() {
          Ok(item) => {
            // Got a result! Stay in Inflight for next call (multishot)
            stream.state = IoStreamState::Inflight { id, receiver };
            Poll::Ready(Some(item))
          }
          Err(std::sync::mpsc::TryRecvError::Empty) => {
            // No result yet, update waker and stay in Inflight
            stream.handle.lio().set_waker(id, cx.waker().clone());
            stream.state = IoStreamState::Inflight { id, receiver };
            Poll::Pending
          }
          Err(std::sync::mpsc::TryRecvError::Disconnected) => {
            // Sender dropped - stream is done
            stream.state = IoStreamState::Done;
            Poll::Ready(None)
          }
        }
      }

      IoStreamState::Done => Poll::Ready(None),
    }
  }
}

/// Drop impl to cancel inflight multishot operations.
impl<T: OpModel> Drop for IoStream<T> {
  fn drop(&mut self) {
    // If the stream is inflight, cancel the operation
    if let IoStreamState::Inflight { id, .. } = self.state {
      self.handle.lio().cancel_stream(id);
    }
  }
}

/// A receiver for streaming operation items.
///
/// Receives items from a streaming operation via a channel.
pub struct StreamReceiver<T> {
  recv: std_mpsc::Receiver<T>,
}

impl<T> StreamReceiver<T> {
  /// Blocks until the next item is available.
  pub fn recv(&self) -> Result<T, std_mpsc::RecvError> {
    self.recv.recv()
  }

  /// Attempts to receive the next item without blocking.
  pub fn try_recv(&self) -> Result<T, std_mpsc::TryRecvError> {
    self.recv.try_recv()
  }

  /// Blocks with a timeout waiting for the next item.
  pub fn recv_timeout(
    &self,
    timeout: Duration,
  ) -> Result<T, std_mpsc::RecvTimeoutError> {
    self.recv.recv_timeout(timeout)
  }

  /// Returns an iterator over stream items.
  pub fn iter(&self) -> impl Iterator<Item = T> + '_ {
    self.recv.iter()
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

  fn test_io(lio: &Lio) -> Io<crate::api::ops::Sleep> {
    api::sleep(Duration::from_millis(1)).with_lio(lio)
  }

  #[test]
  fn test_io_future_completes() {
    let lio = Lio::new(64).unwrap();
    let io = test_io(&lio);
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
  #[should_panic(expected = "IoStreamFuture polled after completion")]
  fn test_io_future_panics_when_polled_after_completion() {
    let lio = Lio::new(64).unwrap();
    let io = test_io(&lio);
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
    let io = test_io(&lio);
    let mut future = io.into_future();

    // Initial state is Pending
    assert!(matches!(future.state, IoStreamFutureState::Pending(_)));

    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);

    // After first poll, state is Inflight
    let _ = Pin::new(&mut future).poll(&mut cx);
    assert!(matches!(future.state, IoStreamFutureState::Inflight { .. }));

    // Complete the operation
    run_until_done(&lio);

    // After completion poll, state is Done
    let _ = Pin::new(&mut future).poll(&mut cx);
    assert!(matches!(future.state, IoStreamFutureState::Done));
  }

  #[test]
  fn test_io_future_multiple_pending_polls() {
    let lio = Lio::new(64).unwrap();
    let io = test_io(&lio);
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
    assert!(matches!(future.state, IoStreamFutureState::Inflight { .. }));

    // Now complete and verify
    run_until_done(&lio);
    let poll3 = Pin::new(&mut future).poll(&mut cx);
    assert!(poll3.is_ready());
  }

  #[test]
  fn test_multiple_futures_can_coexist() {
    let lio = Lio::new(64).unwrap();

    // Create multiple futures from the same lio - this was not possible before
    let fut1 = test_io(&lio).into_future();
    let fut2 = test_io(&lio).into_future();
    let fut3 = test_io(&lio).into_future();

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
    let _fut1 = test_io(&lio1).into_future();
    let _fut2 = test_io(&lio2).into_future();

    // Running on either handle processes completions for both
    run_until_done(&lio1);
  }

  #[test]
  fn test_lio_with_receiver() {
    let lio = Lio::new(64).unwrap();

    // Operations work with .with_lio()
    let mut receiver = test_io(&lio).send();

    // Run the event loop to complete the operation
    run_until_done(&lio);

    let result = receiver.try_recv();
    assert!(result.is_some());
    assert!(result.unwrap().is_ok());
  }

  #[test]
  fn test_stream_send_uses_bound_lio() {
    let lio = Lio::new(64).unwrap();

    let receiver =
      api::interval(Duration::from_millis(1)).with_lio(&lio).send();

    run_until_done(&lio);

    let item = receiver.try_recv();
    assert!(item.is_ok(), "stream receiver did not get first interval tick");
    assert!(item.unwrap().is_ok());
  }
}
