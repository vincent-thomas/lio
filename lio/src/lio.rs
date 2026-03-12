use crate::{
  backends::{IoBackend, OpStore},
  op::Op,
  registration::Registration,
};

use std::{cell::RefCell, io, rc::Rc, task::Waker, time::Duration};

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
}

#[derive(Clone)]
pub struct Lio {
  inner: Rc<RefCell<LioInner>>,
}

#[non_exhaustive]
pub enum Error {
  EntryNotFound,
  EntryNotCompleted,
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
      use crate::backends::pollingv2::Poller;
      Self::new_with_backend(Poller::new(), cap)
    }
    #[cfg(linux)]
    {
      use crate::backends::io_uring::IoUring;
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
  /// use lio::backends::pollingv2::Poller;
  ///
  /// let mut lio = Lio::new_with_backend(Poller::new(), 1024).unwrap();
  /// ```
  pub fn new_with_backend<D>(mut backend: D, cap: usize) -> io::Result<Self>
  where
    D: IoBackend + 'static,
  {
    backend.init(cap)?;

    let inner =
      LioInner { io: Box::new(backend), store: OpStore::with_capacity(cap) };
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

    // Copy completion data to release borrow on inner.io
    let completed: Vec<_> = inner
      .io
      .wait_timeout(timeout)?
      .iter()
      .map(|c| (c.op_id, c.result))
      .collect();

    // Collect IDs to remove (callbacks consume the result, wakers don't)
    let mut to_remove = Vec::new();

    for (op_id, result) in &completed {
      let Some(op) = inner.store.get_mut(*op_id) else {
        panic!("lio bookkeeping bug: completed op doesn't exist in store.");
      };
      op.set_done(*result);

      // If the result was consumed (callback path), mark for removal.
      // Waker path leaves result in place for check_done to consume.
      if op.result_consumed() {
        to_remove.push(*op_id);
      }
    }

    // Remove consumed entries
    for id in to_remove {
      inner.store.remove(id);
    }

    Ok(completed.len())
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

  pub(crate) fn set_waker(&self, id: u64, waker: Waker) {
    let mut inner = self.inner.borrow_mut();
    if let Some(entry) = inner.store.get_mut(id) {
      entry.set_waker(waker);
    }
  }
}
