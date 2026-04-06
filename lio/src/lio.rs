use crate::{
  api::op::{Action, Completion},
  backend::{IoBackend, store::OpStore},
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
  fn dispatch_action(inner: &mut LioInner, id: u64, action: Action) {
    match action {
      Action::Io(op) => inner.io.push(id, op),
      Action::Sleep(duration) => inner.time.schedule(id, duration),
    }
  }

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
    #[cfg(target_os = "linux")]
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

  /// Schedule a StreamOp-based Registration for execution.
  ///
  /// The Registration already contains the StreamOp. This function:
  /// 1. Inserts the Registration to get an operation ID
  /// 2. Calls Registration.next_op(None) to get the first Op to submit
  /// 3. Submits the Op to the backend
  pub(crate) fn schedule(
    &self,
    mut registration: Registration,
  ) -> io::Result<u64> {
    let mut inner = self.inner.borrow_mut();

    let action = registration.action().ok_or_else(|| {
      io::Error::new(
        io::ErrorKind::InvalidInput,
        "OpModel returned no action on first call",
      )
    })?;

    let id = inner.store.insert(registration);
    Self::dispatch_action(&mut inner, id, action);
    Ok(id)
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
      .wait(effective_timeout)?
      .iter()
      .map(|c| (c.registration_id(), c.result()))
      .collect();

    // Collect IDs to remove (callbacks consume the result, wakers don't)
    let mut to_remove = Vec::new();
    let mut to_resubmit: Vec<(u64, Action)> = Vec::new();

    // Process I/O completions
    for (op_id, result) in &completed {
      let Some(op) = inner.store.get_mut(*op_id) else {
        // Op was cancelled and removed - ignore stale completion.
        // This happens with multishot ops on io_uring where cancellation
        // is async and completions may arrive after removal.
        continue;
      };
      // Process completion and get optional next operation to resubmit
      if let Some(next_action) = op.on_completion(Completion::new(*result)) {
        to_resubmit.push((*op_id, next_action));
      }

      // For single-shot ops: remove when result consumed (callback path).
      // For stream ops: remove when done and all results consumed.
      // Waker path leaves result in place for check_done to consume.
      if op.is_finished() {
        to_remove.push(*op_id);
      }
    }

    // Resubmit operations that returned Continue
    for (op_id, next_action) in to_resubmit {
      Self::dispatch_action(&mut inner, op_id, next_action);
    }

    // Process expired timers
    let expired_timers: Vec<_> = inner.time.poll_expired().collect();
    let expired_timer_count = expired_timers.len();
    for timer_id in expired_timers {
      let mut next_action = None;
      let mut finished = false;

      if let Some(reg) = inner.store.get_mut(timer_id) {
        next_action = reg.on_completion(Completion::with_flags(
          SLEEP_RESULT,
          crate::api::op::CompletionFlags::TIMER,
        ));
        finished = reg.is_finished();
      }

      inner.time.remove(timer_id);

      if let Some(action) = next_action {
        Self::dispatch_action(&mut inner, timer_id, action);
      }

      if finished {
        to_remove.push(timer_id);
      }
    }

    // Remove consumed entries
    for id in to_remove {
      inner.store.remove(id);
    }

    Ok(completed.len() + expired_timer_count)
  }

  // NOTE: check_done and check_stream_done are no longer needed with the channel-based
  // architecture. Results are now sent through mpsc channels directly to futures/streams.

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
    inner.time.remove(id);
    inner.store.remove(id);
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::{
    api::ops::Nop,
    backend::{IoBackend, OpCompleted, op::Op},
  };
  use std::sync::{Arc, Mutex};

  #[derive(Default)]
  struct TestBackend {
    queued: Vec<(u64, Op)>,
    completed: Vec<OpCompleted>,
  }

  impl IoBackend for TestBackend {
    fn init(&mut self, _cap: usize) -> io::Result<()> {
      Ok(())
    }

    fn push(&mut self, id: u64, op: Op) {
      self.queued.push((id, op));
    }

    fn flush(&mut self) -> io::Result<()> {
      for (id, op) in self.queued.drain(..) {
        match op {
          Op::Nop => self.completed.push(OpCompleted::new(id, 0)),
          _ => unreachable!("test backend only supports Nop"),
        }
      }
      Ok(())
    }

    fn wait(
      &mut self,
      _timeout: Option<Duration>,
    ) -> io::Result<&[OpCompleted]> {
      Ok(&self.completed)
    }
  }

  #[test]
  fn schedule_and_run_completes_nop_callback() {
    let lio = Lio::new_with_backend(TestBackend::default(), 8).unwrap();
    let received = Arc::new(Mutex::new(Vec::new()));
    let out = Arc::clone(&received);
    let reg = Registration::new_callback(
      move |item| out.lock().unwrap().push(item),
      Box::new(Nop),
    );

    let _id = lio.schedule(reg).unwrap();

    let processed = lio.try_run().unwrap();
    assert_eq!(processed, 1);

    let items = received.lock().unwrap();
    assert_eq!(items.len(), 1);
    assert!(matches!(items[0], Ok(())));
  }
}
