//! `lio`-provided [`IoBackend`] impl for `io_uring`.

use crate::{
  backends::{
    IoBackend, IoDriver, IoSubmitter, OpCompleted, OpStore, SubmitErr,
  },
  operation::Operation,
};
use std::{io, sync::Arc};

pub struct IoUring;

impl IoBackend for IoUring {
  type Driver = IoUringDriver;
  type Submitter = IoUringSubmitter;
  type State = IoUringState;

  fn new_state() -> io::Result<Self::State> {
    Ok(IoUringState { _uring: io_uring::IoUring::new(128)? })
  }

  fn new(
    state: Arc<Self::State>,
  ) -> io::Result<(Self::Submitter, Self::Driver)> {
    let (_, sq, cq) = state._uring.split();
    let handle = IoUringDriver { cq, state: Arc::clone(&state) };
    let submitter = IoUringSubmitter { sq, state: Arc::clone(&state) };

    Ok((submitter, handle))
  }
}

pub struct IoUringSubmitter {
  sq: io_uring::SubmissionQueue<'static>,
  state: Arc<IoUringState>,
}

unsafe impl Send for IoUringSubmitter {}
unsafe impl Send for IoUringDriver {}

pub struct IoUringDriver {
  cq: io_uring::CompletionQueue<'static>,
  state: Arc<IoUringState>,
}
pub struct IoUringState {
  _uring: io_uring::IoUring,
}

fn into_shared<'a>(ptr: *const ()) -> &'a IoUringState {
  unsafe { &*(ptr as *const IoUringState) }
}

impl IoSubmitter for IoUringSubmitter {
  fn submit(&mut self, id: u64, op: &dyn Operation) -> Result<(), SubmitErr> {
    // Convert OpId to u64 for io_uring user_data
    let entry = op.create_entry().user_data(id);

    unsafe { self.sq.push(&entry) }.map_err(|_| SubmitErr::Full)?;

    self.sq.sync();
    // TODO: Make more efficient
    self.state._uring.submit().map_err(SubmitErr::Io)?;
    self.sq.sync();

    Ok(())
  }

  // Submit a NOP operation to wake up submit_and_wait
  // Use a special user_data value (u64::MAX) that won't match any real operation
  // Submit a NOP operation to wake up submit_and_wait
  // Use a special user_data value that won't match any real operation
  fn notify(&mut self) -> Result<(), SubmitErr> {
    let nop_entry = io_uring::opcode::Nop::new().build().user_data(u64::MAX);
    unsafe { self.sq.push(&nop_entry) }.map_err(|_| SubmitErr::Full)?;

    self.sq.sync();
    // Submit to wake up any blocked submit_and_wait
    self.state._uring.submit()?;
    self.sq.sync();
    Ok(())
  }
}

impl IoDriver for IoUringDriver {
  fn try_tick(&mut self, _s: &OpStore) -> io::Result<Vec<OpCompleted>> {
    self.tick_inner(false)
  }
  fn tick(&mut self, _s: &OpStore) -> io::Result<Vec<OpCompleted>> {
    self.tick_inner(true)
  }
}

impl IoUringDriver {
  pub fn tick_inner(
    &mut self,
    can_block: bool,
  ) -> io::Result<Vec<OpCompleted>> {
    self.cq.sync();
    self.state._uring.submit_and_wait(if can_block { 1 } else { 0 })?;
    self.cq.sync();

    let mut op_c = Vec::new();

    for io_entry in &mut self.cq {
      let operation_id = io_entry.user_data();
      let result = io_entry.result();
      // io_uring returns negative errno on error, positive result on success
      op_c.push(OpCompleted::new(operation_id, result as isize));
    }

    Ok(op_c)
  }
}

#[cfg(test)]
test_io_driver!(IoUring);
