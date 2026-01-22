use crate::{
  backends::{IoBackend, OpStore},
  operation::OperationExt,
  registration::StoredOp,
};

use std::{io, task::Waker, time::Duration};

pub struct Lio {
  store: OpStore,
  io: Box<dyn IoBackend>,
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
  /// ```ignore
  /// let mut lio = Lio::new(1024)?;
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
    type Backend = crate::backends::pollingv2::Poller;
    #[cfg(linux)]
    type Backend = crate::backends::io_uring::IoUring;

    Self::new_with_backend(Backend::new(), cap)
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
  /// ```ignore
  /// use lio::backends::impls::pollingv2::Poller;
  ///
  /// let mut lio = Lio::new_with_backend(Poller::new(), 1024)?;
  /// ```
  pub fn new_with_backend<D>(mut backend: D, cap: usize) -> io::Result<Self>
  where
    D: IoBackend + 'static,
  {
    backend.init(cap)?;

    Ok(Self { io: Box::new(backend), store: OpStore::with_capacity(cap) })
  }

  pub fn schedule(&mut self, stored: StoredOp) -> io::Result<u64> {
    // Inserting first because of a stable pointer to push is required.
    let id = self.store.insert(stored);

    let Some(registration) = self.store.get(id) else {
      self.store.remove(id);
      panic!("literally just inserted it");
    };

    match self.io.push(id, registration.op_ref()) {
      Ok(()) => Ok(id),
      Err(err) => {
        self.store.remove(id);
        Err(err)
      }
    }
  }

  /// Non-blocking poll for completed operations.
  ///
  /// Returns immediately, processing any completions that are ready.
  pub fn try_run(&mut self) -> io::Result<usize> {
    self.run_inner(Some(Duration::ZERO))
  }

  /// Block until at least one operation completes.
  pub fn run(&mut self) -> io::Result<usize> {
    self.run_inner(None)
  }

  /// Run the event loop with a timeout.
  ///
  /// Waits for at least one operation to complete or until the timeout expires,
  /// whichever comes first. Returns `Ok(true)` if completions were processed,
  /// `Ok(false)` if the timeout expired with no completions.
  pub fn run_timeout(&mut self, timeout: Duration) -> io::Result<usize> {
    self.run_inner(Some(timeout))
  }

  fn run_inner(&mut self, timeout: Option<Duration>) -> io::Result<usize> {
    self.io.flush()?;
    let completed = self.io.wait_timeout(&mut self.store, timeout)?;

    let len_completed = completed.len();

    for completed_op in completed {
      let Some(op) = self.store.get_mut(completed_op.op_id) else {
        panic!("lio bookkeeping bug: completed op doesn't exist in store.");
      };
      op.set_done(completed_op.result);
    }

    Ok(len_completed)
  }

  pub fn check_done<T>(&mut self, key: u64) -> Result<T::Result, Error>
  where
    T: OperationExt,
  {
    match self.store.get_mut(key) {
      Some(entry) => {
        let result =
          entry.try_extract::<T>().ok_or_else(|| Error::EntryNotCompleted)?;
        self.store.remove(key);
        Ok(result)
      }
      None => Err(Error::EntryNotFound),
    }
  }

  pub fn set_waker(&mut self, id: u64, waker: Waker) -> Option<()> {
    let entry = self.store.get_mut(id)?;
    entry.set_waker(waker);
    Some(())
  }
}
