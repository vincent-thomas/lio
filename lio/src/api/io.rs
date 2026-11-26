//! Operation progress tracking and result consumption.
//!
//! This module provides types for managing I/O operation lifecycles and consuming
//! their results through multiple patterns: async/await, callbacks, blocking calls,
//! and channels.
//!
//! # Overview
//!
//! [`Io<T>`] is a model with the ways to run it. Every function in
//! [`api`](crate::api) returns one, and [`api::run`](crate::api::run) makes
//! one of a model of your own. The result is consumed in one of these ways:
//!
//! - **Async/await**: Implements `IntoFuture`, allowing direct `.await` syntax
//! - **Callbacks**: [`when_done()`](Io::when_done) executes a closure on completion
//! - **Channels**: [`send()`](Io::send) and [`send_with()`](Io::send_with)
//!   deliver results via channels
//!
//! # Architecture
//!
//! ```text
//! Io<T>
//!   ├─> IntoFuture ──> Run<T>      (async/await)
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
//! An [`Io`] stays on the thread of the [`Lio`] that runs it. Results can
//! leave it: [`Receiver`] and [`StreamReceiver`] are `Send`, so an operation's
//! result can be consumed on another thread than the one that started it.

use crate::{
  driver,
  driver::Lio,
  model::{self, OpModel, OpStream, Task},
};

use std::{
  cell::RefCell,
  collections::VecDeque,
  future::Future,
  pin::Pin,
  rc::Rc,
  sync::mpsc as std_mpsc,
  task::{Context, Poll},
  time::Duration,
};

/// A model, with the ways to run it and get its result.
///
/// Every function in [`api`](crate::api) returns one, and
/// [`api::run`](crate::api::run) makes one of a model of your own. It runs
/// on the driver installed for the thread, or on the one
/// [`with_lio`](Self::with_lio) names.
///
/// It is also a model itself, so it composes: see the
/// [`OpModel`] implementation below.
///
/// # Examples
/// See examples in each method's docs.
#[must_use = "Io doesn't schedule any operation on itself."]
pub struct Io<T, D = Installed> {
  op: T,
  driver: D,
}

/// Which driver an [`Io`] or [`IoStream`] runs on: [`Installed`], or a
/// [`Lio`] named with `with_lio`.
pub trait Driver: sealed::Sealed {
  #[doc(hidden)]
  fn into_lio(self) -> Lio;
}

/// The driver installed for the thread, by
/// [`install_global`](crate::install_global).
#[derive(Clone, Copy, Debug, Default)]
pub struct Installed;

mod sealed {
  pub trait Sealed {}
  impl Sealed for super::Installed {}
  impl Sealed for crate::Lio {}
}

impl Driver for Installed {
  #[inline]
  fn into_lio(self) -> Lio {
    match driver::get_global() {
      Some(lio) => lio,
      None => panic!(
        "No Lio instance available. Either call install_global(lio) or use .with_lio(&lio) before consuming the operation."
      ),
    }
  }
}

impl Driver for Lio {
  #[inline]
  fn into_lio(self) -> Lio {
    self
  }
}

/// An `Io` that has not been given a driver is a model like any other: it
/// goes into a [`Join`](crate::model::Join) or a model of your own, which
/// polls it.
///
/// One that names its driver is not, so a driver chosen for a part of a
/// model, which could not be honoured, does not compile:
///
/// ```compile_fail
/// use lio::api;
/// use lio::model::Join;
///
/// let lio = lio::Lio::new(8).unwrap();
/// let _ = Join::new(api::nop().with_lio(&lio), api::nop());
/// ```
impl<T: OpModel> OpModel for Io<T, Installed> {
  type Item = T::Item;

  #[inline]
  fn poll(&mut self, cx: &mut model::Cx<'_>) -> model::Step<Self::Item> {
    self.op.poll(cx)
  }
}

impl<T, D> Io<T, D>
where
  T: OpModel + 'static,
  D: Driver,
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
    T::Item: Send + 'static,
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
    T::Item: Send + 'static,
  {
    // The handle is dropped: the model runs until it is done.
    let _task = self.when_done(move |res| {
      let _ = sender.send(res);
    });
  }

  /// Registers a callback to be invoked when the operation completes.
  ///
  /// The callback runs during [`Lio::run`](crate::Lio::run) (or a sibling),
  /// without the driver locked: it may schedule follow-up operations on the
  /// same `Lio`, drop other in-flight operations, or drive the loop itself.
  ///
  /// Returns a handle to [`cancel`](Task::cancel) the model by. Dropping
  /// the handle leaves it running.
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
  #[inline]
  pub fn when_done<F>(self, f: F) -> Task
  where
    F: FnMut(T::Item) + 'static,
  {
    let (lio, op) = self.into_lio();
    model::spawn(&lio, op, f)
  }
}

/// A blocking receiver for operation results.
///
/// Provides blocking and non-blocking methods to receive operation results.
/// This is used for callback-based I/O completions where results are sent
/// via a channel.
///
/// If the [`Lio`] is dropped before the operation completes, the operation
/// never completes: [`try_recv`](Self::try_recv) and
/// [`recv_timeout`](Self::recv_timeout) keep returning `None`, and
/// [`recv`](Self::recv) panics instead of blocking forever.
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
  /// Panics if the [`Lio`] is dropped before the operation completes, since
  /// the result can then never arrive.
  pub fn recv(mut self) -> T {
    match self.get_inner() {
      Some(value) => match value.recv() {
        Ok(value) => value,
        Err(_) => {
          panic!("lio: the Lio was dropped before the operation completed")
        }
      },
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
          // The Lio was dropped, so the operation never completes: wait out
          // the timeout like any operation still in flight would.
          std_mpsc::RecvTimeoutError::Disconnected => {
            std::thread::sleep(duration);
            self.set_inner(value);
            None
          }
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
          // `Disconnected`: the Lio was dropped, so the operation never
          // completes.
          std_mpsc::TryRecvError::Empty
          | std_mpsc::TryRecvError::Disconnected => {
            self.set_inner(receiver);
            None
          }
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

impl<T> Io<T> {
  /// Creates a new Io from an operation.
  ///
  /// The returned Io has no Lio instance bound. You must call
  /// `.with_lio()` before consuming the operation.
  #[inline]
  pub fn from_op(op: T) -> Self {
    Self { op, driver: Installed }
  }
}

impl<T, D> Io<T, D> {
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
  #[inline]
  pub fn with_lio(self, lio: &Lio) -> Io<T, Lio> {
    Io { op: self.op, driver: lio.clone() }
  }
}

impl<T, D: Driver> Io<T, D> {
  #[inline]
  fn into_lio(self) -> (Lio, T) {
    (self.driver.into_lio(), self.op)
  }
}

impl<T, D> IntoFuture for Io<T, D>
where
  T: OpModel + Unpin + 'static,
  D: Driver,
{
  type Output = T::Item;
  type IntoFuture = model::Run<T>;

  fn into_future(self) -> Self::IntoFuture {
    let (lio, op) = self.into_lio();
    model::Run::new(lio, op)
  }
}

struct LocalFutureSlot<T> {
  item: Option<T>,
  waker: Option<std::task::Waker>,
}

// ============================================================================
// IoStream - Async Stream for multi-item operations
// ============================================================================

/// A stream, with the ways to get its items: [`start`](Self::start) for a
/// handle to take them from, or [`send`](Self::send) and
/// [`send_with`](Self::send_with) for a channel.
///
/// It runs on the driver installed for the thread, or on the one
/// [`with_lio`](Self::with_lio) names. Every method consumes it, so a
/// stream cannot be started twice.
#[must_use = "a stream does nothing until it is started"]
pub struct IoStream<T, D = Installed> {
  op: T,
  driver: D,
}

/// An `IoStream` that has not been given a driver is a stream like any
/// other, to poll from a stream of your own.
impl<T: OpStream> OpStream for IoStream<T, Installed> {
  type Item = T::Item;

  #[inline]
  fn poll_next(&mut self, cx: &mut model::Cx<'_>) -> model::Next<Self::Item> {
    self.op.poll_next(cx)
  }
}

/// The items of a running stream, taken one at a time with
/// [`next`](Self::next).
///
/// The stream runs from the first `next`. After its last item `next` returns
/// `None`. Dropping this stops the stream.
pub struct Items<T>
where
  T: OpStream,
{
  state: ItemsState<T>,
  lio: Lio,
}

enum ItemsState<T>
where
  T: OpStream,
{
  Pending(T),
  Inflight { task: Task, slot: Rc<RefCell<StreamSlot<T::Item>>> },
  Done,
}

struct StreamSlot<T> {
  slot: LocalFutureSlot<T>,
  queued: VecDeque<T>,
  ended: bool,
}

impl<T> StreamSlot<T> {
  fn push_item(&mut self, item: T) {
    if self.slot.item.is_some() {
      self.queued.push_back(item);
    } else {
      self.slot.item = Some(item);
    }
  }

  fn pop_item(&mut self) -> Option<T> {
    let item = self.slot.item.take()?;
    if let Some(next) = self.queued.pop_front() {
      self.slot.item = Some(next);
    }
    Some(item)
  }
}

/// Marks the stream ended when the model's item sink is dropped, which is
/// when the model is.
struct StreamEnd<T> {
  slot: Rc<RefCell<StreamSlot<T>>>,
}

impl<T> Drop for StreamEnd<T> {
  fn drop(&mut self) {
    let waker = {
      let mut wait = self.slot.borrow_mut();
      wait.ended = true;
      // Queued items sit behind `slot.item`, so one waiting item means the
      // consumer will see it on the next poll. The sink already woke.
      if wait.slot.item.is_some() {
        return;
      }
      wait.slot.waker.take()
    };
    if let Some(waker) = waker {
      waker.wake();
    }
  }
}

impl<T> IoStream<T> {
  pub fn from_op(op: T) -> Self {
    Self { op, driver: Installed }
  }
}

impl<T, D> IoStream<T, D> {
  /// Runs the stream on `lio` instead of the driver installed for the
  /// thread.
  pub fn with_lio(self, lio: &Lio) -> IoStream<T, Lio> {
    IoStream { op: self.op, driver: lio.clone() }
  }
}

impl<T, D> IoStream<T, D>
where
  T: OpStream + 'static,
  D: Driver,
{
  /// Gives the handle the stream's items are taken from, one
  /// [`next`](Items::next) at a time.
  pub fn start(self) -> Items<T> {
    Items { state: ItemsState::Pending(self.op), lio: self.driver.into_lio() }
  }

  /// Runs the stream and calls `f` with every item.
  ///
  /// Returns a handle to [`cancel`](Task::cancel) the stream by. Dropping
  /// the handle leaves it running until it ends.
  pub fn for_each<F>(self, f: F) -> Task
  where
    F: FnMut(T::Item) + 'static,
  {
    let lio = self.driver.into_lio();
    model::spawn_stream(&lio, self.op, f)
  }

  /// Runs the stream and gives a receiver for its items.
  pub fn send(self) -> StreamReceiver<T::Item>
  where
    T::Item: Send,
  {
    let (sender, receiver) = std_mpsc::channel();
    self.send_with(sender);
    StreamReceiver { recv: receiver }
  }

  /// Sends every item through `sender`. The stream stops once every
  /// receiver of `sender` is dropped.
  pub fn send_with(self, sender: std_mpsc::Sender<T::Item>)
  where
    T::Item: Send,
  {
    let lio = self.driver.into_lio();
    // The handle is dropped: the stream runs until it ends or nobody
    // receives.
    let _task = model::spawn_while(&lio, model::Many(self.op), move |item| {
      sender.send(item).is_ok()
    });
  }
}

impl<T> Items<T>
where
  T: OpStream + 'static,
{
  /// The next item, or `None` once the stream has ended.
  pub async fn next(&mut self) -> Option<T::Item> {
    ItemsNextFuture { stream: self }.await
  }
}

struct ItemsNextFuture<'a, T: OpStream> {
  stream: &'a mut Items<T>,
}

impl<T: OpStream + 'static> Future for ItemsNextFuture<'_, T> {
  type Output = Option<T::Item>;

  fn poll(
    mut self: Pin<&mut Self>,
    cx: &mut Context<'_>,
  ) -> Poll<Self::Output> {
    let this = &mut *self;
    let stream = &mut *this.stream;

    let (task, slot) =
      match std::mem::replace(&mut stream.state, ItemsState::Done) {
        ItemsState::Pending(op) => {
          let lio = stream.lio.clone();
          if lio.is_full() {
            // Wait for a slot; the driver wakes us when one frees up.
            lio.wait_for_capacity(cx.waker());
            stream.state = ItemsState::Pending(op);
            return Poll::Pending;
          }
          let slot = Rc::new(RefCell::new(StreamSlot {
            slot: LocalFutureSlot { item: None, waker: None },
            queued: VecDeque::new(),
            ended: false,
          }));
          let sink = Rc::clone(&slot);
          let end = StreamEnd { slot: Rc::clone(&slot) };
          let task = model::spawn_stream(&lio, op, move |item| {
            let _end = &end;
            let waker = {
              let mut wait = sink.borrow_mut();
              wait.push_item(item);
              wait.slot.waker.take()
            };
            // Wake with the slot released: the waker may poll this stream
            // inline, and that poll borrows the slot.
            if let Some(waker) = waker {
              waker.wake();
            }
          });
          // The model may have produced items, or finished, already.
          (task, slot)
        }
        ItemsState::Inflight { task, slot } => (task, slot),
        ItemsState::Done => return Poll::Ready(None),
      };

    let mut wait = slot.borrow_mut();
    if let Some(item) = wait.pop_item() {
      drop(wait);
      stream.state = ItemsState::Inflight { task, slot };
      Poll::Ready(Some(item))
    } else if wait.ended {
      Poll::Ready(None)
    } else {
      match &wait.slot.waker {
        Some(waker) if waker.will_wake(cx.waker()) => {}
        _ => wait.slot.waker = Some(cx.waker().clone()),
      }
      drop(wait);
      stream.state = ItemsState::Inflight { task, slot };
      Poll::Pending
    }
  }
}

impl<T: OpStream> Drop for Items<T> {
  fn drop(&mut self) {
    if let ItemsState::Inflight { task, slot } = &self.state {
      // Nobody is left to wake.
      slot.borrow_mut().slot.waker = None;
      task.stop();
    }
  }
}

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
  use crate::api::ops;
  use crate::backend::testing::ScriptedBackend;
  use crate::model::{Cx, Next};
  use crate::ready;
  use std::sync::Arc;
  use std::sync::atomic::{AtomicUsize, Ordering};
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
    // SAFETY: the raw waker uses a static no-op vtable whose functions ignore
    // the null data pointer, so it is valid to construct a `Waker` from it.
    unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) }
  }

  /// Helper to run lio until completion (with timeout to avoid hanging)
  fn run_until_done(lio: &Lio) {
    use std::time::Duration;
    // Use try_run first (immediate completions), then short timeout
    lio.try_run().unwrap();
    lio.run_timeout(Duration::from_millis(10)).unwrap();
  }

  fn test_io(lio: &Lio) -> Io<crate::api::ops::Sleep, Lio> {
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
  #[should_panic(expected = "model future polled after completion")]
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

    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);

    // After first poll, state is Inflight
    let _ = Pin::new(&mut future).poll(&mut cx);

    // Complete the operation
    run_until_done(&lio);

    // After completion poll, state is Done
    let _ = Pin::new(&mut future).poll(&mut cx);
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

  fn scripted(cap: usize) -> (ScriptedBackend, Lio) {
    let backend = ScriptedBackend::new();
    (backend.clone(), Lio::new_with_backend(backend, cap).unwrap())
  }

  /// Counts the clones and wakes of the wakers made by [`counting_waker`].
  #[derive(Default)]
  struct Counts {
    clones: AtomicUsize,
    wakes: AtomicUsize,
  }

  fn counting_waker(counts: &Arc<Counts>) -> Waker {
    static VTABLE: RawWakerVTable =
      RawWakerVTable::new(clone, wake, wake_by_ref, drop);

    /// # Safety
    /// `data` came from `Arc::into_raw` on an `Arc<Counts>`.
    unsafe fn counts_of<'a>(data: *const ()) -> &'a Counts {
      // SAFETY: per the caller, `data` points to a live `Counts`.
      unsafe { &*data.cast::<Counts>() }
    }
    unsafe fn clone(data: *const ()) -> RawWaker {
      // SAFETY: the waker's data is an `Arc<Counts>` it holds a count of.
      unsafe {
        counts_of(data).clones.fetch_add(1, Ordering::SeqCst);
        Arc::increment_strong_count(data.cast::<Counts>());
      }
      RawWaker::new(data, &VTABLE)
    }
    unsafe fn wake(data: *const ()) {
      // SAFETY: as in `clone`; waking consumes the waker's count.
      unsafe {
        wake_by_ref(data);
        drop(data);
      }
    }
    unsafe fn wake_by_ref(data: *const ()) {
      // SAFETY: as in `clone`.
      unsafe { counts_of(data).wakes.fetch_add(1, Ordering::SeqCst) };
    }
    unsafe fn drop(data: *const ()) {
      // SAFETY: as in `clone`; this releases the waker's count.
      unsafe { Arc::decrement_strong_count(data.cast::<Counts>()) };
    }

    let data = Arc::into_raw(Arc::clone(counts)).cast::<()>();
    // SAFETY: the vtable keeps one `Arc` count per waker.
    unsafe { Waker::from_raw(RawWaker::new(data, &VTABLE)) }
  }

  #[test]
  fn a_future_wakes_the_waker_it_was_last_polled_with() {
    let (backend, lio) = scripted(1);
    let mut future = api::nop().with_lio(&lio).into_future();
    let (first, second) = (Arc::default(), Arc::default());
    let a = counting_waker(&first);
    assert!(
      Pin::new(&mut future).poll(&mut Context::from_waker(&a)).is_pending()
    );
    let clones = first.clones.load(Ordering::SeqCst);
    assert!(
      Pin::new(&mut future).poll(&mut Context::from_waker(&a)).is_pending()
    );
    assert_eq!(
      first.clones.load(Ordering::SeqCst),
      clones,
      "polled again with the same waker, the future keeps the one it has"
    );

    let b = counting_waker(&second);
    assert!(
      Pin::new(&mut future).poll(&mut Context::from_waker(&b)).is_pending()
    );
    backend.complete_all(0);
    lio.try_run().unwrap();
    assert_eq!(first.wakes.load(Ordering::SeqCst), 0);
    assert_eq!(second.wakes.load(Ordering::SeqCst), 1);
    assert!(
      Pin::new(&mut future).poll(&mut Context::from_waker(&b)).is_ready()
    );
  }

  #[test]
  fn recv_timeout_returns_a_completed_result() {
    let (backend, lio) = scripted(1);
    let mut receiver = api::nop().with_lio(&lio).send();
    assert!(receiver.recv_timeout(Duration::ZERO).is_none());
    backend.complete_all(0);
    lio.try_run().unwrap();
    let result = receiver.recv_timeout(Duration::from_secs(5));
    assert!(result.is_some_and(|result| result.is_ok()));
  }

  /// Yields 1, 2 and 3, one nop before each.
  struct Counter(u32, Io<ops::Nop>);

  impl Counter {
    fn new() -> Self {
      Self(0, api::nop())
    }
  }

  impl OpStream for Counter {
    type Item = u32;

    fn poll_next(&mut self, cx: &mut Cx<'_>) -> Next<u32> {
      if self.0 == 3 {
        return Next::End;
      }
      let _ = ready!(self.1.poll(cx));
      self.0 += 1;
      if self.0 < 3 {
        self.1 = api::nop();
      }
      Next::Item(self.0)
    }
  }

  #[test]
  fn stream_receiver_iterates_every_item_until_the_stream_ends() {
    let lio = Lio::new_with_backend(ScriptedBackend::auto(0), 1).unwrap();
    let receiver = IoStream::from_op(Counter::new()).with_lio(&lio).send();
    for _ in 0..3 {
      lio.try_run().unwrap();
    }
    assert_eq!(receiver.iter().collect::<Vec<_>>(), [1, 2, 3]);
  }

  #[test]
  fn a_stream_wakes_the_waker_it_was_last_polled_with() {
    let (backend, lio) = scripted(1);
    let mut stream = IoStream::from_op(Counter::new()).with_lio(&lio).start();
    let mut next = Box::pin(stream.next());
    let (first, second) = (Arc::default(), Arc::default());
    let a = counting_waker(&first);
    assert!(next.as_mut().poll(&mut Context::from_waker(&a)).is_pending());
    let b = counting_waker(&second);
    assert!(next.as_mut().poll(&mut Context::from_waker(&b)).is_pending());

    backend.complete_all(0);
    lio.try_run().unwrap();
    assert_eq!(first.wakes.load(Ordering::SeqCst), 0);
    assert_eq!(second.wakes.load(Ordering::SeqCst), 1);
    let item = next.as_mut().poll(&mut Context::from_waker(&b));
    assert_eq!(item, Poll::Ready(Some(1)));
  }

  #[test]
  fn a_stream_polled_again_with_the_same_waker_keeps_it() {
    let (_backend, lio) = scripted(1);
    let mut stream = IoStream::from_op(Counter::new()).with_lio(&lio).start();
    let mut next = Box::pin(stream.next());
    let counts = Arc::default();
    let waker = counting_waker(&counts);
    let mut cx = Context::from_waker(&waker);
    assert!(next.as_mut().poll(&mut cx).is_pending());
    let clones = counts.clones.load(Ordering::SeqCst);
    assert!(next.as_mut().poll(&mut cx).is_pending());
    assert_eq!(
      counts.clones.load(Ordering::SeqCst),
      clones,
      "polled again with the same waker, the stream keeps the one it has"
    );
  }

  /// Takes a step without an item (unless that is already done), then
  /// finishes with 7.
  struct AgainThenDone(bool, Io<ops::Nop>, bool);

  impl AgainThenDone {
    fn new(stepped: bool) -> Self {
      Self(stepped, api::nop(), false)
    }
  }

  impl OpStream for AgainThenDone {
    type Item = u32;

    fn poll_next(&mut self, cx: &mut Cx<'_>) -> Next<u32> {
      if self.2 {
        return Next::End;
      }
      loop {
        let _ = ready!(self.1.poll(cx));
        if std::mem::replace(&mut self.0, true) {
          self.2 = true;
          return Next::Item(7);
        }
        self.1 = api::nop();
      }
    }
  }

  #[test]
  fn a_stream_step_without_an_item_keeps_the_waker_for_the_next() {
    let (backend, lio) = scripted(1);
    let mut stream =
      IoStream::from_op(AgainThenDone::new(false)).with_lio(&lio).start();
    let mut next = Box::pin(stream.next());
    let counts = Arc::default();
    let waker = counting_waker(&counts);
    assert!(next.as_mut().poll(&mut Context::from_waker(&waker)).is_pending());
    for _ in 0..2 {
      backend.complete_all(0);
      lio.try_run().unwrap();
    }
    assert_eq!(counts.wakes.load(Ordering::SeqCst), 1, "woken for the item");
    let item = next.as_mut().poll(&mut Context::from_waker(&waker));
    assert_eq!(item, Poll::Ready(Some(7)));
  }

  /// Polls its stream inline when woken, as an executor that runs tasks from
  /// `wake` would: takes the item, then polls for the next one with `next`.
  struct InlineNext {
    stream: std::cell::RefCell<Items<AgainThenDone>>,
    items: std::cell::RefCell<Vec<Option<u32>>>,
    next: Waker,
  }

  // SAFETY: the test drives everything on one thread.
  unsafe impl Send for InlineNext {}
  // SAFETY: as above.
  unsafe impl Sync for InlineNext {}

  impl std::task::Wake for InlineNext {
    fn wake(self: Arc<Self>) {
      let mut stream = self.stream.borrow_mut();
      let mut cx = Context::from_waker(&self.next);
      for _ in 0..2 {
        if let Poll::Ready(item) =
          Box::pin(stream.next()).as_mut().poll(&mut cx)
        {
          self.items.borrow_mut().push(item);
        }
      }
    }
  }

  #[test]
  fn a_stream_polled_inline_after_its_last_item_is_woken_at_its_end() {
    let (backend, lio) = scripted(1);
    let counts = Arc::default();
    let inline = Arc::new(InlineNext {
      stream: IoStream::from_op(AgainThenDone::new(true))
        .with_lio(&lio)
        .start()
        .into(),
      items: Default::default(),
      next: counting_waker(&counts),
    });
    let waker = Waker::from(inline.clone());
    let first = Box::pin(inline.stream.borrow_mut().next())
      .as_mut()
      .poll(&mut Context::from_waker(&waker));
    assert!(first.is_pending());

    // The item wakes `inline`, which takes it and polls again while the
    // stream's channel is still open.
    backend.complete_all(0);
    lio.try_run().unwrap();
    assert_eq!(*inline.items.borrow(), [Some(7)]);
    assert_eq!(counts.wakes.load(Ordering::SeqCst), 1, "woken at the end");
    let end = Box::pin(inline.stream.borrow_mut().next())
      .as_mut()
      .poll(&mut Context::from_waker(&inline.next));
    assert_eq!(end, Poll::Ready(None));
  }

  #[test]
  fn a_stream_keeps_items_that_arrive_before_they_are_polled() {
    let lio = Lio::new_with_backend(ScriptedBackend::auto(0), 1).unwrap();
    let mut stream = IoStream::from_op(Counter::new()).with_lio(&lio).start();
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);
    assert!(Box::pin(stream.next()).as_mut().poll(&mut cx).is_pending());
    for _ in 0..3 {
      lio.try_run().unwrap();
    }
    let mut items = Vec::new();
    loop {
      match Box::pin(stream.next()).as_mut().poll(&mut cx) {
        Poll::Ready(Some(item)) => items.push(item),
        Poll::Ready(None) => break,
        Poll::Pending => panic!("a stream item was dropped"),
      }
    }
    assert_eq!(items, [1, 2, 3]);
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
