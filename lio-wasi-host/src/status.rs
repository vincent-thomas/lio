//! How a stream ended, for the `future` WASI pairs with it.
//!
//! One value goes from the stream to that future, on the host's thread.
//! What carries it is used again by the next stream, so a stream costs no
//! allocation for it.

use std::cell::{Cell, RefCell};
use std::future::Future;
use std::mem::ManuallyDrop;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

use crate::local::Local;

/// What the two ends share.
pub(crate) struct Shared<E> {
  how: Cell<Option<Result<(), E>>>,
  /// Wakes the future when the stream has ended.
  waker: Cell<Option<Waker>>,
  /// The stream is gone. Without a status, nothing will ever arrive.
  closed: Cell<bool>,
}

/// An error type streams end with: where the cells its streams have used
/// wait for the next ones.
pub(crate) trait Ends: Sized + 'static {
  fn spare<R>(with: impl FnOnce(&RefCell<Vec<Rc<Shared<Self>>>>) -> R) -> R;
}

/// Keeps `$error`'s spare cells on the host's thread.
macro_rules! ends {
  ($error:ty) => {
    impl $crate::status::Ends for $error {
      fn spare<R>(
        with: impl FnOnce(
          &std::cell::RefCell<Vec<std::rc::Rc<$crate::status::Shared<Self>>>>,
        ) -> R,
      ) -> R {
        thread_local! {
          static SPARE: std::cell::RefCell<
            Vec<std::rc::Rc<$crate::status::Shared<$error>>>,
          > = const { std::cell::RefCell::new(Vec::new()) };
        }
        SPARE.with(with)
      }
    }
  };
}

/// How many cells wait for a next stream.
const SPARE: usize = 64;

/// One end. The last to go leaves the cell for the next stream.
struct End<E: Ends>(ManuallyDrop<Local<Rc<Shared<E>>>>);

impl<E: Ends> End<E> {
  fn shared(&self) -> &Shared<E> {
    &self.0
  }
}

impl<E: Ends> Drop for End<E> {
  fn drop(&mut self) {
    // SAFETY: taken once, here, and not used again.
    let shared = unsafe { ManuallyDrop::take(&mut self.0) };
    if Rc::strong_count(&shared) > 1 {
      return;
    }
    // What is left in it is dropped here, not by whoever uses it next.
    drop(shared.how.take());
    drop(shared.waker.take());
    shared.closed.set(false);
    E::spare(|spare| {
      let mut spare = spare.borrow_mut();
      if spare.len() < SPARE {
        spare.push(Rc::clone(&shared));
      }
    });
  }
}

/// The stream's end: says once how it ended.
pub(crate) struct Sender<E: Ends>(End<E>);

/// The future's end.
pub(crate) struct Status<E: Ends>(End<E>);

/// A status and what reports it.
pub(crate) fn channel<E: Ends>() -> (Sender<E>, Status<E>) {
  let shared =
    E::spare(|spare| spare.borrow_mut().pop()).unwrap_or_else(|| {
      Rc::new(Shared {
        how: Cell::new(None),
        waker: Cell::new(None),
        closed: Cell::new(false),
      })
    });
  let other = Rc::clone(&shared);
  (
    Sender(End(ManuallyDrop::new(Local::new(shared)))),
    Status(End(ManuallyDrop::new(Local::new(other)))),
  )
}

impl<E: Ends> Sender<E> {
  /// Says how the stream ended.
  pub(crate) fn send(self, how: Result<(), E>) {
    self.0.shared().how.set(Some(how));
    // Dropping this end wakes the future.
  }
}

impl<E: Ends> Drop for Sender<E> {
  fn drop(&mut self) {
    let shared = self.0.shared();
    shared.closed.set(true);
    if let Some(waker) = shared.waker.take() {
      waker.wake();
    }
  }
}

impl<E: Ends> Status<E> {
  /// A status that is already known.
  pub(crate) fn ready(how: Result<(), E>) -> Self {
    let (sender, status) = channel();
    sender.send(how);
    status
  }
}

impl<E: Ends> Future for Status<E> {
  type Output = wasmtime::Result<Result<(), E>>;

  fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
    let shared = self.0.shared();
    if let Some(how) = shared.how.take() {
      return Poll::Ready(Ok(how));
    }
    if shared.closed.get() {
      return Poll::Ready(Err(wasmtime::Error::msg(
        "a stream went away without saying how it ended",
      )));
    }
    shared.waker.set(Some(cx.waker().clone()));
    Poll::Pending
  }
}

ends!(crate::bindings::wasi::filesystem::types::ErrorCode);
ends!(crate::bindings::wasi::sockets::types::ErrorCode);
ends!(crate::bindings::wasi::cli::types::ErrorCode);
