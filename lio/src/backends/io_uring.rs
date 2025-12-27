use crate::{
  backends::{IoDriver, IoHandler, IoSubmitter, OpCompleted, SubmitErr},
  op::Operation,
  store::OpStore,
};
use std::io;

pub struct IoUring;

impl IoDriver for IoUring {
  type Handler = IoUringHandler;
  type Submitter = IoUringSubmitter;

  fn new_state() -> io::Result<*const ()> {
    let uring = io_uring::IoUring::new(128)?;
    let ptr = Box::new(IoUringState { _uring: uring });
    Ok(Box::into_raw(ptr) as *const _)
  }

  fn drop_state(state: *const ()) {
    let ptr = unsafe { Box::from_raw(state as *mut IoUringState) };
    drop(ptr);
  }

  fn new(ptr: *const ()) -> io::Result<(Self::Submitter, Self::Handler)> {
    let tet = unsafe { &mut *(ptr as *mut IoUringState) };
    let (_, sq, cq) = tet._uring.split();
    let handle = IoUringHandler { cq };
    let submitter = IoUringSubmitter { sq };

    Ok((submitter, handle))
  }
}

pub struct IoUringSubmitter {
  sq: io_uring::SubmissionQueue<'static>,
}
pub struct IoUringHandler {
  cq: io_uring::CompletionQueue<'static>,
}
pub struct IoUringState {
  // subm: io_uring::Submitter<'static>,
  _uring: io_uring::IoUring,
}

//   fn drop(&mut self) {
//     impl Drop for IoUringState {
//     // SAFETY: We leaked this pointer in new(), so we need to reclaim it here
//     // let _ = unsafe { Box::from_raw(self._uring) };
//   }
// }

fn into_shared<'a>(ptr: *const ()) -> &'a IoUringState {
  unsafe { &*(ptr as *const IoUringState) }
}

impl IoSubmitter for IoUringSubmitter {
  fn submit(
    &mut self,
    state: *const (),
    id: u64,
    op: &dyn Operation,
  ) -> Result<(), SubmitErr> {
    // Convert OpId to u64 for io_uring user_data
    let entry = op.create_entry().user_data(id);

    unsafe { self.sq.push(&entry) }.map_err(|_| SubmitErr::Full)?;

    self.sq.sync();
    // TODO: Make more efficient
    into_shared(state)._uring.submit().map_err(SubmitErr::Io)?;
    self.sq.sync();

    Ok(())
  }

  // Submit a NOP operation to wake up submit_and_wait
  // Use a special user_data value (u64::MAX) that won't match any real operation
  // Submit a NOP operation to wake up submit_and_wait
  // Use a special user_data value that won't match any real operation
  fn notify(&mut self, state: *const ()) -> Result<(), SubmitErr> {
    let state = into_shared(state);
    let nop_entry = io_uring::opcode::Nop::new().build().user_data(u64::MAX);
    unsafe { self.sq.push(&nop_entry) }.map_err(|_| SubmitErr::Full)?;

    self.sq.sync();
    // Submit to wake up any blocked submit_and_wait
    state._uring.submit()?;
    self.sq.sync();
    Ok(())
  }
}

impl IoHandler for IoUringHandler {
  fn try_tick(
    &mut self,
    state: *const (),
    _s: &OpStore,
  ) -> io::Result<Vec<super::OpCompleted>> {
    self.tick_inner(false, into_shared(state))
  }
  fn tick(
    &mut self,
    state: *const (),
    _s: &OpStore,
  ) -> io::Result<Vec<super::OpCompleted>> {
    self.tick_inner(true, into_shared(state))
  }
}

impl IoUringHandler {
  pub fn tick_inner(
    &mut self,
    can_block: bool,
    state: &IoUringState,
  ) -> io::Result<Vec<super::OpCompleted>> {
    state._uring.submit_and_wait(if can_block { 1 } else { 0 })?;

    let mut op_c = Vec::new();

    for io_entry in &mut self.cq {
      let operation_id = io_entry.user_data();
      let result = io_entry.result();
      op_c.push(OpCompleted::new(
        operation_id,
        if result < 0 { Err(io::Error::last_os_error()) } else { Ok(result) },
      ));
    }

    Ok(op_c)
  }
}

// impl IoBackend for IoUring {
//   fn submit(&self, op: &dyn Operation, id: u64) -> Result<(), SubmitErr> {
//     // if T::entry_supported(&self.probe) {
//     // let operation_id = store.next_id();
//     let entry = op.create_entry().user_data(id);
//
//     // Insert the operation into wakers first
//     // store.insert(id, op);
//
//     // Then submit to io_uring
//     // SAFETY: because of references rules, a "fake" lock has to be implemented here, but because
//     // of it, this is safe.
//     let _g = self.submission_guard.lock();
//     unsafe {
//       let mut sub = self.inner.submission_shared();
//       // FIXME
//       sub.push(&entry).map_err(|_| SubmitErr::Full)?;
//       sub.sync();
//       drop(sub);
//     }
//     drop(_g);
//
//     self.inner.submit()?;
//     Ok(())
//     // OperationProgress::<T>::new_store_tracked(operation_id)
//     // } else {
//     //   self.polling.submit(op, store)
//     // }
//   }
//
//   fn notify(&self) {
//     // Submit a NOP operation to wake up submit_and_wait
//     // Use a special user_data value (u64::MAX) that won't match any real operation
//     // Submit a NOP operation to wake up submit_and_wait
//     // Use a special user_data value that won't match any real operation
//     let nop_entry = io_uring::opcode::Nop::new().build().user_data(u64::MAX);
//
//     let _g = self.submission_guard.lock();
//     unsafe {
//       let mut sub = self.inner.submission_shared();
//       // If queue is full, just skip - the tick will happen soon anyway
//       let _ = sub.push(&nop_entry);
//       sub.sync();
//     }
//     drop(_g);
//
//     // Submit to wake up any blocked submit_and_wait
//     let _ = self.inner.submit();
//   }
//
//   fn tick(&self, store: &OpStore, can_wait: bool) {
//     self.polling.tick(store, false);
//
//     self.inner.submit_and_wait(if can_wait { 1 } else { 0 }).unwrap();
//
//     // SAFETY: lio guarrantees that only one tick impl is running at any time.
//     for io_entry in unsafe { self.inner.completion_shared() } {
//       let operation_id = io_entry.user_data();
//
//       // If the operation id is not registered (e.g., wake-up NOP), skip.
//       let exists = store.get_mut(operation_id, |entry| {
//         entry.set_done(Self::from_i32_to_io_result(io_entry.result()))
//       });
//
//       assert!(exists.is_some());
//       assert!(store.remove(operation_id));
//     }
//     unsafe { self.inner.completion_shared() }.sync();
//   }
// }
