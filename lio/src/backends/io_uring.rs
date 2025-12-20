use super::{IoBackend, OpStore};
use crate::op_registration::OpNotification;
use crate::sync::Mutex;
use crate::{OperationProgress, op};
use std::io;

pub struct IoUring {
  inner: io_uring::IoUring,
  probe: io_uring::Probe,
  submission_guard: Mutex<()>,
  polling: super::pollingv2::Poller,
}

impl IoUring {
  pub fn new() -> io::Result<Self> {
    let (io_uring, probe) = {
      let io_uring = io_uring::IoUring::new(256)?;
      let mut probe = io_uring::Probe::new();
      io_uring.submitter().register_probe(&mut probe)?;
      (io_uring, probe)
    };

    Ok(Self {
      inner: io_uring,
      probe,
      submission_guard: Mutex::new(()),
      polling: super::pollingv2::Poller::new()?,
    })
  }
  pub fn from_i32_to_io_result(res: i32) -> io::Result<i32> {
    if res < 0 { Err(io::Error::from_raw_os_error(res)) } else { Ok(res) }
  }
}

impl IoBackend for IoUring {
  fn submit<T>(&self, op: T, store: &OpStore) -> OperationProgress<T>
  where
    T: op::Operation,
  {
    if T::entry_supported(&self.probe) {
      let operation_id = store.next_id();
      let mut op = Box::new(op);
      let entry = op.create_entry().user_data(operation_id);

      // Insert the operation into wakers first
      store.insert(operation_id, op);

      // Then submit to io_uring
      // SAFETY: because of references rules, a "fake" lock has to be implemented here, but because
      // of it, this is safe.
      let _g = self.submission_guard.lock();
      unsafe {
        let mut sub = self.inner.submission_shared();
        // FIXME
        sub.push(&entry).expect("unwrapping for now");
        sub.sync();
        drop(sub);
      }
      drop(_g);

      self.inner.submit().unwrap();
      OperationProgress::<T>::new_store_tracked(operation_id)
    } else {
      self.polling.submit(op, store)
    }
  }

  fn notify(&self) {
    // Submit a NOP operation to wake up submit_and_wait
    // Use a special user_data value (u64::MAX) that won't match any real operation
    // Submit a NOP operation to wake up submit_and_wait
    // Use a special user_data value that won't match any real operation
    let nop_entry = io_uring::opcode::Nop::new().build().user_data(u64::MAX);

    let _g = self.submission_guard.lock();
    unsafe {
      let mut sub = self.inner.submission_shared();
      // If queue is full, just skip - the tick will happen soon anyway
      let _ = sub.push(&nop_entry);
      sub.sync();
    }
    drop(_g);

    // Submit to wake up any blocked submit_and_wait
    let _ = self.inner.submit();
  }

  fn tick(&self, store: &OpStore, can_wait: bool) {
    self.polling.tick(store, false);

    self.inner.submit_and_wait(if can_wait { 1 } else { 0 }).unwrap();

    // SAFETY: lio guarrantees that only one tick impl is running at any time.
    for io_entry in unsafe { self.inner.completion_shared() } {
      let operation_id = io_entry.user_data();

      // If the operation id is not registered (e.g., wake-up NOP), skip.
      let Some(set_done_result) = store.get_mut(operation_id, |entry| {
        entry.set_done(Self::from_i32_to_io_result(io_entry.result()))
      }) else {
        continue;
      };

      // if should keep.
      match set_done_result {
        None => {}
        Some(value) => match value {
          OpNotification::Waker(waker) => waker.wake(),
          OpNotification::Callback(callback) => {
            store.get_mut(operation_id, |entry| callback.call(entry));
            assert!(store.remove(operation_id));
          }
        },
      }
    }
    unsafe { self.inner.completion_shared() }.sync();
  }
}
