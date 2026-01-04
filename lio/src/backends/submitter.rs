use std::sync::Arc;

use crate::{
  backends::SubmitErr, op::Operation, registration::StoredOp, store::OpStore,
};

#[derive(Debug)]
pub struct OpCompleted {
  pub(crate) op_id: u64,
  /// Result of the operation:
  /// - >= 0 on success (the result value)
  /// - < 0 on error (negative errno value)
  pub(crate) result: isize,
}

impl OpCompleted {
  pub fn new(op_id: u64, result: isize) -> Self {
    Self { op_id, result }
  }
}

pub trait IoSubmitter {
  /// Caller can assume op is in the OpStore.
  fn submit(
    &mut self,
    state: *const (),
    id: u64,
    op: &dyn Operation,
  ) -> Result<(), SubmitErr>;

  /// TODO: move to already existing id api.
  fn notify(&mut self, state: *const ()) -> Result<(), SubmitErr>;
}

pub struct Submitter {
  sub: Box<dyn IoSubmitter>,
  store: Arc<OpStore>,
  state: *const (),
}

unsafe impl Send for Submitter {}

impl Submitter {
  pub fn new(
    sub: Box<dyn IoSubmitter>,
    store: Arc<OpStore>,
    state: *const (),
  ) -> Self {
    Self { sub, store, state }
  }

  pub fn submit(&mut self, op: StoredOp) -> Result<u64, SubmitErr> {
    let id = self.store.insert(op);
    let _ref = self
      .store
      .get(id, |_r| self.sub.submit(self.state, id, _r.op_ref()))
      .expect("what");

    match _ref {
      Ok(()) => Ok(id),
      Err(err) => match err {
        SubmitErr::NotCompatible => {
          // This backend doesn't support this operation
          // We need to extract the StoredOp back from the store
          // For now, panic since this should only happen with primary worker,
          // not the blocking worker which handles everything
          panic!(
            "Backend returned NotCompatible after operation was inserted. \
             This should only happen with the primary worker, and the operation \
             should be retried with the blocking worker."
          );
        }
        _ => {
          assert!(self.store.remove(id));
          Err(err)
        }
      },
    }
  }

  pub fn notify(&mut self) -> Result<(), SubmitErr> {
    self.sub.notify(self.state)
  }
}
