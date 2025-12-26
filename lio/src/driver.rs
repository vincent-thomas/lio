use crate::OperationProgress;
use crate::backends::{self, IoBackend};
#[cfg(feature = "buf")]
use crate::buf::{BufStore, LentBuf};
use crate::op::{Operation, OperationExt};
use crate::sync::Mutex;

use std::task::Waker;
use std::{
  collections::HashMap,
  ptr::NonNull,
  sync::{
    OnceLock,
    atomic::{AtomicPtr, AtomicU64, Ordering},
    mpsc,
  },
  thread::JoinHandle,
};
use std::{fmt, io};

use crate::op;
use crate::op_registration::{OpRegistration, StoredOp};

pub struct OpStore {
  store: Mutex<HashMap<u64, OpRegistration>>,
}

impl OpStore {
  fn new() -> OpStore {
    Self { store: Mutex::new(HashMap::new()) }
  }
  pub fn next_id(&self) -> u64 {
    static NEXT: OnceLock<AtomicU64> = OnceLock::new();
    NEXT.get_or_init(|| AtomicU64::new(0)).fetch_add(1, Ordering::AcqRel)
  }
  pub fn remove(&self, id: u64) -> bool {
    let mut _lock = self.store.lock();
    _lock.remove(&id).is_some()
  }
  pub fn get_mut<F, R>(&self, id: u64, mut _f: F) -> Option<R>
  where
    F: FnOnce(&mut OpRegistration) -> R,
  {
    let mut _lock = self.store.lock();
    let reg = _lock.get_mut(&id)?;
    Some(_f(reg))
  }
  pub fn insert(&self, id: u64, stored: StoredOp) {
    let mut _lock = self.store.lock();
    let result = _lock.insert(id, OpRegistration::new(stored));
    assert!(
      result.is_none(),
      "OpStore::insert: operation ID {} already exists",
      id
    );
    drop(_lock);
  }
}

#[cfg(linux)]
pub type Default = backends::IoUring;
#[cfg(not(linux))]
pub type Default = backends::pollingv2::Poller;

pub struct Driver<Io = Default> {
  driver: Io,
  store: OpStore,
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
    // Check if already initialized
    let current = DRIVER.load(Ordering::Acquire);
    if !current.is_null() {
      return Err(TryInitError::AlreadyInit);
    }

    let driver_ptr = Box::into_raw(Box::new(Driver {
      #[cfg(feature = "buf")]
      buf_store: BufStore::default(),
      driver: Default::new().map_err(TryInitError::Io)?,
      store: OpStore::new(),
      worker_shutdown: Mutex::new(None),
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
        let _ = unsafe { Box::from_raw(driver_ptr) };
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

  pub(crate) fn submit(stored: StoredOp) -> u64 {
    let driver = Driver::get();
    let id = driver.store.next_id();
    driver.driver.submit(stored.op_ref(), id).unwrap();
    driver.store.insert(id, stored);

    id
    // OperationProgress::StoreTracked { id }
    // driver.driver.submit(op, &driver.store)
  }

  pub(crate) fn tick(&self, can_wait: bool) {
    self.driver.tick(&self.store, can_wait)
  }

  /// Spawns a background worker thread that continuously processes I/O operations.
  ///
  /// The worker thread will call `tick(true)` in a loop, blocking until operations complete.
  /// This allows async operations to complete without manual `tick()` calls.
  ///
  /// Returns an error if a worker is already running.
  pub fn spawn_worker(&'static self) -> Result<(), &'static str> {
    let mut worker_guard = self.worker_thread.lock();

    if worker_guard.is_some() {
      return Err("Worker thread already running");
    }

    let (shutdown_tx, shutdown_rx) = mpsc::channel();

    let handle = utils::create_worker(move || {
      loop {
        // Check for shutdown signal (non-blocking)
        match shutdown_rx.try_recv() {
          Ok(()) => break,
          Err(mpsc::TryRecvError::Disconnected) => break,
          Err(mpsc::TryRecvError::Empty) => {}
        }

        // Process I/O operations (blocking wait)
        self.tick(true);
      }
    });
    *self.worker_shutdown.lock() = Some(shutdown_tx);
    *worker_guard = Some(handle);

    Ok(())
  }

  /// Stops the background worker thread gracefully.
  ///
  /// Sends a shutdown signal and waits for the thread to finish.
  /// Does nothing if no worker is running.
  pub fn stop_worker(&self) {
    // Send shutdown signal
    if let Some(tx) = self.worker_shutdown.lock().take() {
      let _ = tx.send(()); // Ignore error if receiver already dropped
      self.driver.notify(); // Wake up the worker if it's blocked in tick()
    }

    // Wait for thread to finish
    if let Some(handle) = self.worker_thread.lock().take() {
      let _ = handle.join(); // Ignore join errors
    }
  }

  pub fn spawn_ev(&'static self, sender: mpsc::Receiver<()>) {
    let _ = sender;
    // let handle = utils::create_worker(move || {
    //   loop {
    //     match sender.try_recv() {
    //       Ok(()) => break,
    //       Err(err) => match err {
    //         mpsc::TryRecvError::Empty => {}
    //         mpsc::TryRecvError::Disconnected => {
    //           break;
    //         }
    //       },
    //     }
    //
    //     self.driver.tick(&self.store, true);
    //   }
    // });
    // let old = self.background_handle.lock().replace(handle);
    // assert!(old.is_none());
  }

  // FIXME: On first run per key, run run_blocking and that will fix it.
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
  // pub fn set_callback<T, F>(&self, id: u64, callback: F)
  // where
  //   T: op::Operation,
  //   F: FnOnce(T::Result) + Send,
  // {
  //   use crate::op_registration::OpCallback;
  //   self
  //     .store
  //     .get_mut(id, |entry| {
  //       entry.set_callback(OpCallback::new::<T, F>(callback))
  //     })
  //     .unwrap()
  // }

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
