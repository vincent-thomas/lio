use crate::backends::SubmitErr;
#[cfg(feature = "buf")]
use crate::buf::{BufStore, LentBuf};
use crate::store::OpStore;
use crate::sync::Mutex;
use crate::{
  backends::{self, Handler, IoDriver, IoHandler, IoSubmitter, Submitter},
  op::OperationExt,
};

use std::sync::Arc;
use std::task::Waker;
use std::{fmt, io};
use std::{
  ptr::NonNull,
  sync::{
    atomic::{AtomicPtr, Ordering},
    mpsc,
  },
  thread::JoinHandle,
};

use crate::registration::StoredOp;

pub struct Driver {
  submitter: Submitter,
  store: Arc<OpStore>,
  worker_shutdown: Mutex<Option<mpsc::Sender<()>>>,
  worker_thread: Mutex<Option<JoinHandle<()>>>,
  #[cfg(feature = "buf")]
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
  pub(crate) fn try_init() -> Result<(), TryInitError> {
    #[cfg(linux)]
    pub type Default = backends::IoUring;
    #[cfg(not(linux))]
    pub type Default = backends::pollingv2::Poller;
    Self::try_init_with_driver::<Default>()
  }
  pub(crate) fn try_init_with_driver<D>() -> Result<(), TryInitError>
  where
    D: IoDriver,
  {
    // Check if already initialized
    if !DRIVER.load(Ordering::Acquire).is_null() {
      return Err(TryInitError::AlreadyInit);
    }
    let state = D::new_state().map_err(TryInitError::Io)?;
    let (subm, handler) = D::new(state).map_err(TryInitError::Io)?;

    let store = Arc::new(OpStore::new());
    // TODO
    let handler = Handler::new(Box::new(handler), store.clone(), state);

    let driver_ptr = Box::into_raw(Box::new(Driver {
      #[cfg(feature = "buf")]
      buf_store: BufStore::default(),
      submitter: Submitter::new(Box::new(subm), store.clone(), state),
      worker_shutdown: Mutex::new(None),
      store: store.clone(),
      worker_thread: Mutex::new(None),
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

        drop(handler);

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

  pub fn exit() {
    let ptr = NonNull::new(DRIVER.swap(std::ptr::null_mut(), Ordering::AcqRel))
      .expect("driver not initialized");
    Self::deallocate(ptr);
  }

  #[cfg(feature = "buf")]
  pub(crate) fn try_lend_buf(&'static self) -> Option<LentBuf<'static>> {
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
    panic!();
    // let driver = Driver::get();
    // driver.submitter.submit(stored)
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

mod utils {
  use std::thread;

  pub fn create_worker<F, T>(handle: F) -> thread::JoinHandle<T>
  where
    F: FnOnce() -> T,
    F: Send + 'static,
    T: Send + 'static,
  {
    thread::Builder::new()
      .name("lio".into())
      .spawn(handle)
      .expect("failed to launch the worker thread")
  }
}
// /// Spawns a background worker thread that continuously processes I/O operations.
// ///
// /// The worker thread will call `tick(true)` in a loop, blocking until operations complete.
// /// This allows async operations to complete without manual `tick()` calls.
// ///
// /// Returns an error if a worker is already running.
// pub fn spawn_worker(&'static self) -> Result<(), &'static str> {
//   let mut worker_guard = self.worker_thread.lock();
//
//   if worker_guard.is_some() {
//     return Err("Worker thread already running");
//   }
//
//   let (shutdown_tx, shutdown_rx) = mpsc::channel();
//
//   let handle = utils::create_worker(move || {
//     loop {
//       // Check for shutdown signal (non-blocking)
//       match shutdown_rx.try_recv() {
//         Ok(()) => break,
//         Err(mpsc::TryRecvError::Disconnected) => break,
//         Err(mpsc::TryRecvError::Empty) => {}
//       }
//
//       // Process I/O operations (blocking wait)
//       self.tick(true);
//     }
//   });
//   *self.worker_shutdown.lock() = Some(shutdown_tx);
//   *worker_guard = Some(handle);
//
//   Ok(())
// }

// /// Stops the background worker thread gracefully.
// ///
// /// Sends a shutdown signal and waits for the thread to finish.
// /// Does nothing if no worker is running.
// pub fn stop_worker(&self) {
//   // Send shutdown signal
//   if let Some(tx) = self.worker_shutdown.lock().take() {
//     let _ = tx.send(()); // Ignore error if receiver already dropped
//     self.driver.notify(); // Wake up the worker if it's blocked in tick()
//   }
//
//   // Wait for thread to finish
//   if let Some(handle) = self.worker_thread.lock().take() {
//     let _ = handle.join(); // Ignore join errors
//   }
// }
