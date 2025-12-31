use crate::backends::SubmitErr;
use crate::buf::{BufStore, LentBuf};
use crate::store::OpStore;
use crate::worker::Worker;
use crate::{
  backends::{self, Handler, IoDriver, Submitter},
  op::OperationExt,
};

use std::sync::Arc;
use std::task::Waker;
use std::{fmt, io};
use std::{
  ptr::NonNull,
  sync::atomic::{AtomicPtr, Ordering},
};

use crate::registration::StoredOp;

pub struct Driver {
  store: Arc<OpStore>,
  primary_worker: Worker,
  blocking_worker: Worker,
  buf_store: BufStore,
}

#[derive(Debug)]
pub enum TryInitError {
  AlreadyInit,
  Io(io::Error),
}

impl std::error::Error for TryInitError {}

impl fmt::Display for TryInitError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::AlreadyInit => f.write_str("TryInitError"),
      Self::Io(io_err) => io_err.fmt(f),
    }
  }
}

static DRIVER: AtomicPtr<Driver> = AtomicPtr::new(std::ptr::null_mut());

impl Driver {
  pub(crate) fn try_init_with_capacity_and_bufstore<D>(
    cap: usize,
    buf_store: BufStore,
  ) -> Result<(), TryInitError>
  where
    D: IoDriver,
  {
    // Check if already initialized
    if !DRIVER.load(Ordering::Acquire).is_null() {
      return Err(TryInitError::AlreadyInit);
    }

    // Initialize primary backend
    let primary_state = D::new_state().map_err(TryInitError::Io)?;
    let (primary_subm, primary_handler) =
      D::new(primary_state).map_err(TryInitError::Io)?;

    // Initialize blocking backend
    let blocking_state = backends::blocking::BlockingBackend::new_state()
      .map_err(TryInitError::Io)?;
    let (blocking_subm, blocking_handler) =
      backends::blocking::BlockingBackend::new(blocking_state)
        .map_err(TryInitError::Io)?;

    let store = Arc::new(OpStore::with_capacity(cap));

    let primary_handler =
      Handler::new(Box::new(primary_handler), store.clone(), primary_state);
    let primary_submitter =
      Submitter::new(Box::new(primary_subm), store.clone(), primary_state);

    let blocking_handler =
      Handler::new(Box::new(blocking_handler), store.clone(), blocking_state);
    let blocking_submitter =
      Submitter::new(Box::new(blocking_subm), store.clone(), blocking_state);

    let driver_ptr = Box::into_raw(Box::new(Driver {
      buf_store,
      primary_worker: Worker::spawn(primary_submitter, primary_handler),
      blocking_worker: Worker::spawn(blocking_submitter, blocking_handler),
      store: store.clone(),
    }));

    // Try to set the driver pointer atomically
    match DRIVER.compare_exchange(
      std::ptr::null_mut(),
      driver_ptr,
      Ordering::AcqRel,
      Ordering::Acquire,
    ) {
      Ok(_) => Ok(()),
      Err(_) => {
        // Another thread initialized first, clean up our allocation
        // This drops the driver and its workers
        let _ = unsafe { Box::from_raw(driver_ptr) };

        // Clean up the states
        D::drop_state(primary_state);
        backends::blocking::BlockingBackend::drop_state(blocking_state);
        Err(TryInitError::AlreadyInit)
      }
    }
  }

  pub fn get() -> &'static Driver {
    let ptr = DRIVER.load(Ordering::Acquire);

    assert!(
      !ptr.is_null(),
      "Driver not initialized. Call Driver::init() first."
    );

    // SAFETY: The pointer is valid from init() until deallocate()
    unsafe { &*ptr }
  }

  pub fn exit(&'static self) {
    let ptr = NonNull::new(DRIVER.swap(std::ptr::null_mut(), Ordering::AcqRel))
      .expect("driver not initialized");
    Self::deallocate(ptr);
  }

  pub(crate) fn try_lend_buf<'a>(&'a self) -> Option<LentBuf<'a>> {
    self.buf_store.try_get()
  }

  /// Deallocates the Driver, freeing all resources.
  /// This will panic if the driver is not initialized or if shutdown has not been called first.
  pub(crate) fn deallocate(ptr: NonNull<Driver>) {
    // SAFETY: This pointer was created via Box::into_raw in init()
    let _ = unsafe { Box::from_raw(ptr.as_ptr()) };
  }

  #[allow(unused)]
  pub(crate) fn detach(&self, id: u64) -> Option<()> {
    let _ = id;
    todo!();
  }

  pub(crate) fn submit(stored: StoredOp) -> Result<u64, SubmitErr> {
    let driver = Driver::get();

    // Try primary worker first
    match driver.primary_worker.submit(stored) {
      Ok(id) => Ok(id),
      Err(backends::SubmitErrExt::NotCompatible(op)) => {
        // Fallback to blocking worker for incompatible operations
        match driver.blocking_worker.submit(op) {
          Ok(id) => Ok(id),
          Err(backends::SubmitErrExt::NotCompatible(_)) => {
            // Blocking backend should never return NotCompatible
            panic!("blocking backend returned NotCompatible")
          }
          Err(backends::SubmitErrExt::SubmitErr(e)) => Err(e),
        }
      }
      Err(backends::SubmitErrExt::SubmitErr(e)) => Err(e),
    }
  }

  pub(crate) fn tick(&self, _can_wait: bool) {
    // self.driver.tick(&self.store, can_wait)
  }

  pub fn check_done<T>(&self, key: u64) -> Option<T::Result>
  where
    T: OperationExt,
  {
    match self.store.get_mut(key, |entry| entry.try_extract::<T>()).unwrap() {
      Some(res) => {
        self.store.remove(key);
        Some(res)
      }
      None => None,
    }
  }

  pub(crate) fn set_waker(&self, id: u64, waker: Waker) {
    self.store.get_mut(id, |entry| entry.set_waker(waker)).unwrap()
  }
}
