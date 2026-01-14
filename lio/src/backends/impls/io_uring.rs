//! `lio`-provided [`IoBackend`] impl for `io_uring`.

use lio_uring::{completion::CompletionQueue, submission::SubmissionQueue};

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
  type State = ();

  fn new_state() -> io::Result<Self::State> {
    Ok(())
  }

  fn new(_: Arc<Self::State>) -> io::Result<(Self::Submitter, Self::Driver)> {
    let (sq, cq) = lio_uring::with_capacity(1024)?;

    Ok((IoUringSubmitter { sq }, IoUringDriver { cq }))
  }
}

pub struct IoUringSubmitter {
  sq: SubmissionQueue,
}

unsafe impl Send for IoUringSubmitter {}
unsafe impl Send for IoUringDriver {}

pub struct IoUringDriver {
  cq: CompletionQueue,
}

impl IoSubmitter for IoUringSubmitter {
  fn submit(&mut self, id: u64, op: &dyn Operation) -> Result<(), SubmitErr> {
    // Convert OpId to u64 for io_uring user_data
    let entry = op.create_entry();

    unsafe { self.sq.push(entry, id) }.map_err(|_| SubmitErr::Full)?;

    // TODO: Make more efficient
    self.sq.submit().map_err(SubmitErr::Io)?;

    Ok(())
  }

  // Submit a NOP operation to wake up next
  fn notify(&mut self) -> Result<(), SubmitErr> {
    let nop_entry = lio_uring::operation::Nop::new().build();
    unsafe { self.sq.push(nop_entry, u64::MAX) }
      .map_err(|_| SubmitErr::Full)?;

    // Submit to wake up any blocked submit_and_wait
    self.sq.submit()?;
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
    let mut total_completions = vec![];
    if can_block {
      // next blocks.
      let first = self.cq.next()?;
      total_completions.push(first);
    } else {
      match self.cq.try_next()? {
        Some(op) => {
          total_completions.push(op);
        }
        None => return Ok(vec![]),
      }
    };

    while let Ok(Some(operation)) = self.cq.try_next() {
      total_completions.push(operation);
    }

    let op_completed = total_completions
      .iter()
      .into_iter()
      .map(|op| OpCompleted::new(op.user_data(), op.result() as isize))
      .collect();

    Ok(op_completed)
  }
}

#[cfg(test)]
test_io_driver!(IoUring);
