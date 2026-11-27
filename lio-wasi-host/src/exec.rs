//! Runs a future on the calling thread, with lio as what it waits on.

use std::future::Future;
use std::io;
use std::pin::pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll, Wake, Waker};

use std::cell::RefCell;

use lio::Lio;

thread_local! {
  /// The driver [`block_on`] is running on this thread.
  static DRIVER: RefCell<Option<Lio>> = const { RefCell::new(None) };
}

/// Puts back the driver that was current before a [`block_on`].
struct Restore(Option<Lio>);

impl Drop for Restore {
  fn drop(&mut self) {
    DRIVER.with(|driver| *driver.borrow_mut() = self.0.take());
  }
}

/// Lets the driver submit what was started and take in what has completed,
/// without waiting, if that is likely to finish the operation asking.
///
/// A submission costs a kernel entry unless it shares one. An operation
/// that finishes as soon as it is submitted (file I/O, a socket with data
/// waiting) is worth an entry of its own: it is then done within the host
/// call that started it, and the guest never waits for it, which saves far
/// more than the entry costs. One that has to wait anyway (a timer, an
/// accept, a socket that had nothing last time) is left for the entry that
/// waits, which every operation started since shares.
///
/// An operation calls this once, when its first poll finds it unfinished.
/// Returns whether the driver ran.
pub(crate) fn drive(likely: bool) -> bool {
  likely
    && DRIVER.with(|driver| match driver.try_borrow() {
      Ok(driver) => driver.as_ref().is_some_and(|lio| lio.try_run().is_ok()),
      Err(_) => false,
    })
}

/// Set when the future asked to be polled again.
struct Woken(AtomicBool);

impl Wake for Woken {
  fn wake(self: Arc<Self>) {
    self.0.store(true, Ordering::Release);
  }

  fn wake_by_ref(self: &Arc<Self>) {
    self.0.store(true, Ordering::Release);
  }
}

/// Runs `future` to completion on this thread.
///
/// Whenever the future is waiting, the thread blocks in `lio` until an
/// operation completes. The host's operations use the driver installed for
/// the thread ([`lio::install_global`]), which has to be `lio`.
///
/// # Errors
///
/// When the driver fails.
pub fn block_on<F: Future>(lio: &Lio, future: F) -> io::Result<F::Output> {
  let _restore =
    Restore(DRIVER.with(|driver| driver.replace(Some(lio.clone()))));
  let woken = Arc::new(Woken(AtomicBool::new(true)));
  let waker = Waker::from(Arc::clone(&woken));
  let mut cx = Context::from_waker(&waker);
  let mut future = pin!(future);
  loop {
    if woken.0.swap(false, Ordering::AcqRel)
      && let Poll::Ready(output) = future.as_mut().poll(&mut cx)
    {
      return Ok(output);
    }
    if woken.0.load(Ordering::Acquire) {
      // The future wants another poll: take in what is there, and submit
      // what it started, without waiting.
      lio.try_run()?;
    } else {
      // Nothing more to do until something completes. One kernel entry
      // submits everything started since the last and waits.
      lio.run()?;
    }
  }
}
