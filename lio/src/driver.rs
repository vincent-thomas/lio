use crate::backends::SubmitErr;
use crate::buf::{BufStore, LentBuf};
use crate::store::OpStore;
use crate::worker::Worker;
use crate::{
  backends::{self, Handler, IoDriver, Submitter},
  op::OperationExt,
};

use std::sync::{Arc, Mutex};
use std::task::Waker;
use std::{fmt, io};
use std::{
  ptr::NonNull,
  sync::atomic::{AtomicPtr, Ordering},
};

use crate::registration::StoredOp;

pub struct Driver {
  store: Arc<OpStore>,
  worker: Worker,
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
  pub(crate) fn try_init_with_driver_and_capacity<D>(
    cap: usize,
  ) -> Result<(), TryInitError>
  where
    D: IoDriver,
  {
    // Check if already initialized
    if !DRIVER.load(Ordering::Acquire).is_null() {
      return Err(TryInitError::AlreadyInit);
    }
    let state = D::new_state().map_err(TryInitError::Io)?;
    let (subm, handler) = D::new(state).map_err(TryInitError::Io)?;

    let store = Arc::new(OpStore::with_capacity(cap));

    let handler = Handler::new(Box::new(handler), store.clone(), state);
    let submitter = Submitter::new(Box::new(subm), store.clone(), state);

    let driver_ptr = Box::into_raw(Box::new(Driver {
      buf_store: BufStore::default(),
      worker: Worker::spawn(submitter, handler),
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
        // This drops the state ptr in Submitter::new()
        let _ = unsafe { Box::from_raw(driver_ptr) };

        D::drop_state(state);
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

  pub(crate) fn try_lend_buf(&'static self) -> Option<LentBuf> {
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
    driver.worker.submit(stored)
  }

  pub(crate) fn tick(&self, can_wait: bool) {
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
