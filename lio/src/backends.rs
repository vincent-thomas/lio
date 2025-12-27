use std::{io, sync::Arc};

use crate::{op::Operation, registration::StoredOp, store::OpStore};

#[cfg(linux)]
mod io_uring;
#[cfg(linux)]
pub use io_uring::*;

#[cfg(windows)]
mod iocp;
#[cfg(windows)]
pub use iocp::*;

pub mod pollingv2;
// mod polling;
// pub use polling::*;
// mod threading;
// #[allow(unused_imports)]
// pub use threading::*;

#[derive(Debug)]
pub enum SubmitErr {
  Io(io::Error),
  Full,
  NotCompatible,
}

impl From<io::Error> for SubmitErr {
  fn from(value: io::Error) -> Self {
    Self::Io(value)
  }
}

pub struct OpCompleted {
  op_id: u64,
  result: io::Result<i32>,
}

impl OpCompleted {
  pub fn new(op_id: u64, result: io::Result<i32>) -> Self {
    Self { op_id, result }
  }
}

pub trait IoDriver {
  type Submitter: IoSubmitter + 'static;
  type Handler: IoHandler + 'static;

  fn new_state() -> io::Result<*const ()>;
  fn drop_state(state: *const ());

  fn new(state: *const ()) -> io::Result<(Self::Submitter, Self::Handler)>;
}

pub trait IoSubmitter {
  fn submit(
    &mut self,
    state: *const (),
    id: u64,
    op: &dyn Operation,
  ) -> Result<(), SubmitErr>;

  /// TODO: move to already existing id api.
  fn notify(&mut self, state: *const ()) -> Result<(), SubmitErr>;
}

pub trait IoHandler {
  fn try_tick(
    &mut self,
    state: *const (),
    store: &OpStore,
  ) -> io::Result<Vec<OpCompleted>>;
  fn tick(
    &mut self,
    state: *const (),
    store: &OpStore,
  ) -> io::Result<Vec<OpCompleted>>;
}

pub struct Submitter {
  sub: Box<dyn IoSubmitter>,
  store: Arc<OpStore>,
  state: *const (),
}

impl Submitter {
  pub fn new(
    sub: Box<dyn IoSubmitter>,
    store: Arc<OpStore>,
    state: *const (),
  ) -> Self {
    Self { sub, store, state }
  }

  pub fn submit(&mut self, op: StoredOp) -> Result<u64, SubmitErr> {
    self.store.insert_with_try(|id| {
      self.sub.submit(self.state, id, op.op_ref())?;
      Ok::<_, SubmitErr>(op)
    })
  }
}

pub struct Handler {
  io: Box<dyn IoHandler>,
  state: *const (),
  store: Arc<OpStore>,
}

impl Handler {
  pub fn new(
    io: Box<dyn IoHandler>,
    store: Arc<OpStore>,
    state: *const (),
  ) -> Self {
    Self { io, store, state }
  }

  pub fn tick(&mut self) -> io::Result<Vec<OpCompleted>> {
    let result = self.io.tick(self.state, &self.store)?;
    assert!(!result.is_empty());
    Ok(result)
  }

  pub fn try_tick(&mut self) -> io::Result<Vec<OpCompleted>> {
    let result = self.io.try_tick(self.state, &self.store)?;
    assert!(!result.is_empty());
    Ok(result)
  }
}

// /// Trait defining the interface for I/O operation execution backends.
// ///
// /// Backends handle the lifecycle of operations from submission through completion,
// /// managing resources and notifying the driver when operations finish.
// ///
// /// # Contract
// ///
// /// Implementations must:
// /// - Eventually complete all submitted operations
// /// - Have operation state synced with [OpStore].
// /// - Deliver notifications to wakers or callbacks when operations complete
// pub trait IoBackend: Send + Sync {
//   /// Processes completed operations and advances the backend's internal state.
//   ///
//   /// `store` is the operation store containing all submitted operations.
//   ///  This method is only allowed to block/wait-for-completion if `can_block`
//   ///  is `true`.
//   ///
//   /// # Contract
//   ///
//   /// For each completed operation, implementations must:
//   /// 1. Retrieve the operation result
//   /// 2. Call `OpStore::get_mut()` to mark it as done via `set_done()`
//   /// 3. Handle any extracted notifications (wakers or callbacks)
//   /// 4. Clean up backend-specific resources
//   fn tick(&self, store: &OpStore, can_wait: bool);
//
//   /// Submits an operation to the backend for execution.
//   ///
//   /// `op` is the operation to execute and `store` is the operation store where
//   /// in-progress operations are tracked.
//   ///
//   /// # Returns
//   ///
//   /// Returns an `OperationProgress` indicating how the operation will be handled.
//   ///
//   /// # Contract
//   ///
//   /// Implementations must:
//   /// - Either complete the operation immediately or arrange for it to complete asynchronously
//   /// - If completing asynchronously, insert the operation into the `OpStore` with a unique ID
//   /// - Return an appropriate `OperationProgress` matching the chosen completion path
//   fn submit(&self, op: &dyn Operation, id: u64) -> Result<(), SubmitErr>;
//
//   /// Wakes up a potentially blocking `tick()` call.
//   ///
//   /// # Contract
//   ///
//   /// Implementations must cause any concurrent `tick()` call with `can_wait = true`
//   /// to return, even if no operations have completed.
//   fn notify(&self);
// }
