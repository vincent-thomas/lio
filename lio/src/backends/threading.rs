use std::{
  sync::{
    Arc,
    mpsc::{self, Receiver, Sender},
  },
  thread::{self, JoinHandle},
};

use crate::sync::Mutex;

use crate::{
  OperationProgress, backends::IoBackend, driver::OpStore, op::Operation,
  op_registration::ExtractedOpNotification,
};

/// Work item sent to worker threads
/// The runner executes the operation and sends the completion
struct WorkItem {
  /// The closure that runs the operation and sends completion
  /// This is monomorphized per operation type O
  runner: Box<dyn FnOnce() + Send>,
}

/// Completion sent back from worker threads
struct Completion {
  id: u64,
  result: std::io::Result<i32>,
}

pub struct Threading {
  /// Channel for sending work to thread pool
  work_tx: Sender<WorkItem>,
  /// Channel for sending completions from workers
  completion_tx: Sender<Completion>,
  /// Channel for receiving completions from thread pool
  completion_rx: Mutex<Receiver<Completion>>,
  /// Worker thread handles
  _workers: Vec<JoinHandle<()>>,
  /// Reference to OpStore (set during initialization by Driver)
  store: Option<&'static OpStore>,
}

impl Threading {
  pub fn new() -> Self {
    let worker_count =
      std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);

    let (work_tx, work_rx) = mpsc::channel::<WorkItem>();
    let (completion_tx, completion_rx) = mpsc::channel::<Completion>();

    // Shared work queue for all workers
    let work_rx = Arc::new(Mutex::new(work_rx));

    let mut workers = Vec::with_capacity(worker_count);

    // Spawn worker threads
    for worker_id in 0..worker_count {
      let work_rx = Arc::clone(&work_rx);

      let handle = thread::Builder::new()
        .name(format!("lio-worker-{}", worker_id))
        .spawn(move || {
          loop {
            // Get work from queue
            let work = work_rx.lock().recv();

            match work {
              Ok(work_item) => {
                // Execute the work item (runs operation and sends completion)
                (work_item.runner)();
              }
              Err(_) => {
                // Channel closed, exit worker
                break;
              }
            }
          }
        })
        .expect("Failed to spawn worker thread");

      workers.push(handle);
    }

    Self {
      work_tx,
      completion_tx,
      completion_rx: Mutex::new(completion_rx),
      _workers: workers,
      store: None,
    }
  }
}

impl IoBackend for Threading {
  fn tick(&self, store: &OpStore, can_wait: bool) {
    let rx = self.completion_rx.lock();

    // Process all available completions
    loop {
      let completion = if can_wait {
        // Block waiting for at least one completion
        rx.recv().ok()
      } else {
        // Non-blocking poll
        rx.try_recv().ok()
      };

      let Some(completion) = completion else {
        break;
      };

      // Skip dummy wake-up signals
      if completion.id == u64::MAX {
        continue;
      }

      // Set the operation as done with the result
      // The operation was already inserted into OpStore during submit
      let set_done_result =
        store.get_mut(completion.id, |reg| reg.set_done(completion.result));

      // Handle wakers/callbacks
      if let Some(Some(notification)) = set_done_result {
        match notification {
          #[cfg(feature = "high")]
          ExtractedOpNotification::Waker(waker) => waker.wake(),
          ExtractedOpNotification::Callback(callback) => {
            store.get_mut(completion.id, |entry| callback.call(entry));
            assert!(store.remove(completion.id));
          }
        }
      }
    }
  }

  fn submit<O>(&self, op: O, store: &OpStore) -> OperationProgress<O>
  where
    O: Operation + Sized,
  {
    if O::EVENT_TYPE.is_some() {
      panic!("Threading io driver is not compatible with O::EVENT_TYPE");
    }

    let id = store.next_id();

    // Store the operation in OpStore first
    store.insert(id, Box::new(op));

    // Get OpStore reference that workers can use
    // This is a bit tricky - we need to ensure the store lives long enough
    // For now, we'll assume store is 'static (lives in Driver which is static)
    // We transmute to usize so it can be Send
    let store_ptr = store as *const OpStore as usize;

    // Clone the completion channel sender for the worker
    let completion_tx = self.completion_tx.clone();

    // Create a runner that accesses the operation from OpStore and executes it
    let runner = Box::new(move || {
      // SAFETY: OpStore lives in Driver which is static
      // This is safe as long as the worker runs before Driver is dropped
      let store_ref = unsafe { &*(store_ptr as *const OpStore) };

      // Execute the operation via OpRegistration's run_blocking
      let result = store_ref
        .get_mut(id, |entry| entry.run_blocking())
        .expect("Operation not found in store");

      // Send completion back
      let _ = completion_tx.send(Completion { id, result });
    });

    // Send work to thread pool
    self.work_tx.send(WorkItem { runner }).expect("Worker threads died");

    OperationProgress::<O>::new_threaded(id)
  }

  fn notify(&self) {
    // Send a dummy completion to wake up tick()
    let _ = self.completion_rx.lock();
    // We can't easily send a dummy completion without access to the tx
    // This is a limitation of the current design
    // For now, notify is a no-op
  }
}

impl Drop for Threading {
  fn drop(&mut self) {
    // Dropping work_tx will close the channel
    // Workers will exit when they try to receive
  }
}
