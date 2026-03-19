use crate::{
  backend::{IoBackend, op::Op, store::OpStore},
  registration::Registration,
  time::TimeManager,
};

use std::{cell::RefCell, io, rc::Rc, task::Waker, time::Duration};

/// Result code returned to userspace when a sleep timer fires.
/// Matches the expected error codes in TypedOp::extract_result for Sleep.
#[cfg(target_os = "linux")]
const SLEEP_RESULT: isize = -(libc::ETIME as isize);
#[cfg(any(
  target_os = "macos",
  target_os = "freebsd",
  target_os = "dragonfly"
))]
const SLEEP_RESULT: isize = -(libc::ETIMEDOUT as isize);
#[cfg(not(any(
  target_os = "linux",
  target_os = "macos",
  target_os = "freebsd",
  target_os = "dragonfly"
)))]
const SLEEP_RESULT: isize = 0;

thread_local! {
  static GLOBAL_LIO: RefCell<Option<Lio>> = const { RefCell::new(None) };
}

/// Installs a global Lio instance for the current thread.
///
/// After calling this, operations can be used without explicitly calling `.with_lio()`.
/// This is useful for thread-per-core designs where each thread has its own Lio instance.
///
/// # Panics
///
/// Panics if a global Lio is already installed on this thread.
pub fn install_global(lio: Lio) {
  GLOBAL_LIO.with(|global| {
    let mut global = global.borrow_mut();
    if global.is_some() {
      panic!("Global Lio already installed on this thread. Call uninstall_global() first.");
    }
    *global = Some(lio);
  });
}

/// Uninstalls the global Lio instance for the current thread.
///
/// Returns the previously installed Lio, or `None` if no global was installed.
pub fn uninstall_global() -> Option<Lio> {
  GLOBAL_LIO.with(|global| global.borrow_mut().take())
}

/// Returns a clone of the global Lio instance for the current thread.
///
/// Returns `None` if no global Lio has been installed.
pub(crate) fn get_global() -> Option<Lio> {
  GLOBAL_LIO.with(|global| global.borrow().clone())
}

struct LioInner {
  store: OpStore,
  io: Box<dyn IoBackend>,
  time: TimeManager,
}

#[derive(Clone)]
pub struct Lio {
  inner: Rc<RefCell<LioInner>>,
}

#[non_exhaustive]
pub enum Error {
  EntryNotFound,
  EntryNotCompleted,
  /// Stream has finished and all results have been consumed.
  StreamDone,
}

impl Lio {
  /// Creates a new Lio driver with the default backend and specified capacity.
  ///
  /// # Arguments
  ///
  /// * `cap` - The maximum number of concurrent operations
  ///
  /// # Example
  ///
  /// ```
  /// use lio::Lio;
  ///
  /// let mut lio = Lio::new(1024).unwrap();
  /// ```
  pub fn new(cap: usize) -> io::Result<Self> {
    #[cfg(any(
      target_os = "macos",
      target_os = "ios",
      target_os = "tvos",
      target_os = "watchos",
      target_os = "freebsd",
      target_os = "dragonfly",
      target_os = "openbsd",
      target_os = "netbsd"
    ))]
    {
      use crate::backend::pollingv2::Poller;
      Self::new_with_backend(Poller::new(), cap)
    }
    #[cfg(linux)]
    {
      use crate::backend::io_uring::IoUring;
      Self::new_with_backend(IoUring::new(), cap)
    }
  }

  /// Creates a new Lio driver with the specified backend and capacity.
  ///
  /// # Arguments
  ///
  /// * `backend` - The I/O backend implementation (e.g., io_uring, epoll/kqueue)
  /// * `cap` - The maximum number of concurrent operations
  ///
  /// # Example
  ///
  /// ```
  /// use lio::Lio;
  /// use lio::backend::pollingv2::Poller;
  ///
  /// let mut lio = Lio::new_with_backend(Poller::new(), 1024).unwrap();
  /// ```
  pub fn new_with_backend<D>(mut backend: D, cap: usize) -> io::Result<Self>
  where
    D: IoBackend + 'static,
  {
    backend.init(cap)?;

    let inner = LioInner {
      io: Box::new(backend),
      store: OpStore::with_capacity(cap),
      time: TimeManager::with_capacity(cap),
    };
    Ok(Self { inner: Rc::new(RefCell::new(inner)) })
  }

  pub(crate) fn schedule(
    &self,
    op: Op,
    notifier: Registration,
  ) -> io::Result<u64> {
    let mut inner = self.inner.borrow_mut();
    // Inserting first because of a stable pointer to push is required.
    let id = inner.store.insert(notifier);

    // Handle sleep timers via the userspace timing wheel instead of kernel
    if let Op::Sleep { duration, .. } = &op {
      inner.time.schedule(id, *duration);
      return Ok(id);
    }

    match inner.io.push(id, op) {
      Ok(()) => Ok(id),
      Err(err) => {
        assert!(inner.store.remove(id));
        Err(err)
      }
    }
  }

  /// Non-blocking poll for completed operations.
  ///
  /// Returns immediately, processing any completions that are ready.
  pub fn try_run(&self) -> io::Result<usize> {
    self.run_inner(Some(Duration::ZERO))
  }

  /// Block until at least one operation completes.
  pub fn run(&self) -> io::Result<usize> {
    self.run_inner(None)
  }

  /// Run the event loop with a timeout.
  ///
  /// Waits for at least one operation to complete or until the timeout expires,
  /// whichever comes first. Returns `Ok(true)` if completions were processed,
  /// `Ok(false)` if the timeout expired with no completions.
  pub fn run_timeout(&self, timeout: Duration) -> io::Result<usize> {
    self.run_inner(Some(timeout))
  }

  fn run_inner(&self, timeout: Option<Duration>) -> io::Result<usize> {
    let mut inner = self.inner.borrow_mut();
    inner.io.flush()?;

    // Compute effective timeout: min of user timeout and next timer deadline
    let effective_timeout = match (timeout, inner.time.next_deadline()) {
      (Some(user), Some(timer)) => Some(user.min(timer)),
      (Some(user), None) => Some(user),
      (None, Some(timer)) => Some(timer),
      (None, None) => None,
    };

    // Copy completion data to release borrow on inner.io
    let completed: Vec<_> = inner
      .io
      .wait_timeout(effective_timeout)?
      .iter()
      .map(|c| (c.op_id, c.result, c.more))
      .collect();

    // Collect IDs to remove (callbacks consume the result, wakers don't)
    let mut to_remove = Vec::new();

    // Process I/O completions
    for (op_id, result, more) in &completed {
      let Some(op) = inner.store.get_mut(*op_id) else {
        // Op was cancelled and removed - ignore stale completion.
        // This happens with multishot ops on io_uring where cancellation
        // is async and completions may arrive after removal.
        continue;
      };
      op.set_done(*result, *more);

      // For single-shot ops: remove when result consumed (callback path).
      // For stream ops: remove when done and all results consumed.
      // Waker path leaves result in place for check_done to consume.
      if op.is_finished() {
        to_remove.push(*op_id);
      }
    }

    // Process expired timers
    let expired_timers: Vec<_> = inner.time.poll_expired().collect();
    for timer_id in &expired_timers {
      if let Some(op) = inner.store.get_mut(*timer_id) {
        // Timers are single-shot, so more=false
        op.set_done(SLEEP_RESULT, false);
        if op.is_finished() {
          to_remove.push(*timer_id);
        }
      }
      // Remove from TimeManager tracking
      inner.time.remove(*timer_id);
    }

    // Remove consumed entries
    for id in to_remove {
      inner.store.remove(id);
    }

    Ok(completed.len() + expired_timers.len())
  }

  pub(crate) fn check_done(&self, key: u64) -> Result<isize, Error> {
    let mut inner = self.inner.borrow_mut();
    match inner.store.get_mut(key) {
      Some(entry) => {
        let result = entry.try_take_result().ok_or(Error::EntryNotCompleted)?;
        assert!(inner.store.remove(key));
        Ok(result)
      }
      None => Err(Error::EntryNotFound),
    }
  }

  /// Checks for a completed result from a streaming operation.
  ///
  /// Unlike `check_done`, this pops one result from the stream's queue
  /// and only removes the entry when the stream is finished.
  pub(crate) fn check_stream_done(&self, key: u64) -> Result<isize, Error> {
    let mut inner = self.inner.borrow_mut();
    match inner.store.get_mut(key) {
      Some(entry) => {
        // Check if stream is finished (done and no pending results)
        if entry.is_stream_done() {
          assert!(inner.store.remove(key));
          return Err(Error::StreamDone);
        }
        // Try to pop a result from the stream queue
        let result =
          entry.try_take_stream_result().ok_or(Error::EntryNotCompleted)?;
        // Check again if we should clean up after taking this result
        if entry.is_stream_done() {
          assert!(inner.store.remove(key));
        }
        Ok(result)
      }
      None => Err(Error::EntryNotFound),
    }
  }

  pub(crate) fn set_waker(&self, id: u64, waker: Waker) {
    let mut inner = self.inner.borrow_mut();
    if let Some(entry) = inner.store.get_mut(id) {
      entry.set_waker(waker);
    }
  }

  /// Cancel an in-flight streaming operation.
  ///
  /// This is called when a stream is dropped to stop multishot operations.
  pub(crate) fn cancel_stream(&self, id: u64) {
    let mut inner = self.inner.borrow_mut();
    // Cancel at the backend level (io_uring async cancel, or pollingv2 deregister)
    let _ = inner.io.cancel(id);
    // Remove from store
    inner.store.remove(id);
  }
}
