use crate::{
  backends::{self, IoBackend, OpStore, SubmitErr, Submitter},
  operation::OperationExt,
  registration::StoredOp,
  worker::Worker,
};

use std::{
  fmt, io,
  ptr::NonNull,
  sync::{
    Arc,
    atomic::{AtomicPtr, Ordering},
  },
  task::Waker,
};

pub struct Driver {
  store: Arc<OpStore>,
  primary_worker: Worker,
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
  pub(crate) fn try_init_with_capacity<D>(
    cap: usize,
  ) -> Result<(), TryInitError>
  where
    D: IoBackend,
  {
    // Check if already initialized
    if !DRIVER.load(Ordering::Acquire).is_null() {
      return Err(TryInitError::AlreadyInit);
    }

    // Initialize primary backend
    let owned_state = D::new_state().map_err(TryInitError::Io)?;
    let state_ptr = Box::into_raw(Box::new(owned_state));

    // SAFETY: state_ptr was just created from Box::into_raw, so it's valid, properly aligned,
    // and points to initialized memory. We have exclusive access to it.
    let (primary_subm, primary_handler) =
      D::new(unsafe { &mut *state_ptr }).map_err(TryInitError::Io)?;

    let store = Arc::new(OpStore::with_capacity(cap));

    let primary_handler =
      backends::Driver::new(Box::new(primary_handler), store.clone());
    let primary_submitter = Submitter::new(Box::new(primary_subm), store.clone());

    let driver_ptr = Box::into_raw(Box::new(Driver {
      // buf_store,
      primary_worker: Worker::spawn(primary_submitter, primary_handler),
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
        // SAFETY: driver_ptr was created via Box::into_raw just above, and the CAS failed
        // so we own it and must clean it up. It's valid and properly aligned.
        let _ = unsafe { Box::from_raw(driver_ptr) };
        // SAFETY: state_ptr was created via Box::into_raw earlier in this function.
        // Since we're cleaning up after failed initialization, we own it and must drop it.
        let _t = unsafe { Box::from_raw(state_ptr) };

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

    // SAFETY: The pointer is valid from try_init_with_capacity() until deallocate().
    // We just verified it's non-null, and it points to a valid Driver created via Box::into_raw.
    // The 'static lifetime is valid because DRIVER is static and the pointer won't be
    // deallocated until exit() is called.
    unsafe { &*ptr }
  }

  pub fn exit(&'static self) {
    let ptr = NonNull::new(DRIVER.swap(std::ptr::null_mut(), Ordering::AcqRel))
      .expect("driver not initialized");
    Self::deallocate(ptr);
  }

  /// Deallocates the Driver, freeing all resources.
  /// This will panic if the driver is not initialized or if shutdown has not been called first.
  pub(crate) fn deallocate(ptr: NonNull<Driver>) {
    // SAFETY: This pointer was created via Box::into_raw in try_init_with_capacity().
    // The caller (exit) ensures this is only called once, and ptr is guaranteed to be
    // a valid, aligned pointer to a Driver that we own.
    let _ = unsafe { Box::from_raw(ptr.as_ptr()) };
  }

  pub(crate) fn submit(&self, stored: StoredOp) -> Result<u64, SubmitErr> {
    // Try primary worker first
    match self.primary_worker.submit(stored) {
      Ok(id) => Ok(id),
      Err(err) => Err(err),
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
