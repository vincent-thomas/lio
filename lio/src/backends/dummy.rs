//! Dummy IoDriver for testing purposes
//!
//! This is a simple, synchronous implementation that stores operations in a queue
//! and completes them immediately when polled. Useful for testing Worker behavior
//! without dealing with actual I/O operations.

use std::{
  io,
  sync::{Arc, Mutex},
};

use crate::{
  backends::{IoDriver, IoHandler, IoSubmitter, OpCompleted, SubmitErr},
  op::Operation,
  store::OpStore,
};

#[derive(Clone)]
struct DummyState {
  pending: Arc<Mutex<Vec<(u64, i32)>>>,
}

impl DummyState {
  fn new() -> Self {
    Self { pending: Arc::new(Mutex::new(Vec::new())) }
  }
}

pub struct DummyDriver;

impl IoDriver for DummyDriver {
  type Submitter = DummySubmitter;
  type Handler = DummyHandler;

  fn new_state() -> io::Result<*const ()> {
    let state = Box::new(DummyState::new());
    Ok(Box::into_raw(state) as *const ())
  }

  fn drop_state(state: *const ()) {
    unsafe {
      let _ = Box::from_raw(state as *mut DummyState);
    }
  }

  fn new(state: *const ()) -> io::Result<(Self::Submitter, Self::Handler)> {
    let state_ref = unsafe { &*(state as *const DummyState) };
    Ok((
      DummySubmitter { state: state_ref.clone() },
      DummyHandler { state: state_ref.clone() },
    ))
  }
}

pub struct DummySubmitter {
  state: DummyState,
}

impl IoSubmitter for DummySubmitter {
  fn submit(
    &mut self,
    _state: *const (),
    id: u64,
    _op: &dyn Operation,
  ) -> Result<(), SubmitErr> {
    // For dummy driver, we just queue the operation with a successful result
    let mut pending = self.state.pending.lock().unwrap();
    pending.push((id, 0)); // 0 represents success
    Ok(())
  }

  fn notify(&mut self, _state: *const ()) -> Result<(), SubmitErr> {
    // Dummy implementation - nothing to do
    Ok(())
  }
}

pub struct DummyHandler {
  state: DummyState,
}

impl IoHandler for DummyHandler {
  fn try_tick(
    &mut self,
    _state: *const (),
    _store: &OpStore,
  ) -> io::Result<Vec<OpCompleted>> {
    let mut pending = self.state.pending.lock().unwrap();
    let completed = pending
      .drain(..)
      .map(|(op_id, result)| OpCompleted::new(op_id, result as isize))
      .collect();
    Ok(completed)
  }

  fn tick(
    &mut self,
    state: *const (),
    store: &OpStore,
  ) -> io::Result<Vec<OpCompleted>> {
    // For dummy driver, tick and try_tick behave the same
    self.try_tick(state, store)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::{op::nop::Nop, registration::StoredOp};

  #[test]
  fn test_dummy_driver_basic() {
    let state = DummyDriver::new_state().unwrap();
    let (mut submitter, mut handler) = DummyDriver::new(state).unwrap();

    let store = OpStore::new();
    let op = StoredOp::new_waker(Nop);
    let id = store.insert(op);

    // Submit operation
    submitter.submit(state, id, &Nop).unwrap();

    // Tick handler to complete operation
    let completed = handler.try_tick(state, &store).unwrap();

    assert_eq!(completed.len(), 1);
    assert_eq!(completed[0].op_id, id);
    assert!(completed[0].result >= 0);

    DummyDriver::drop_state(state);
  }

  #[test]
  fn test_dummy_driver_multiple_ops() {
    let state = DummyDriver::new_state().unwrap();
    let (mut submitter, mut handler) = DummyDriver::new(state).unwrap();

    let store = OpStore::new();
    let mut ids = Vec::new();

    // Submit 10 operations
    for _ in 0..10 {
      let op = StoredOp::new_waker(Nop);
      let id = store.insert(op);
      submitter.submit(state, id, &Nop).unwrap();
      ids.push(id);
    }

    // Tick handler to complete all operations
    let completed = handler.try_tick(state, &store).unwrap();

    assert_eq!(completed.len(), 10);
    for (i, comp) in completed.iter().enumerate() {
      assert_eq!(comp.op_id, ids[i]);
      assert!(comp.result >= 0);
    }

    DummyDriver::drop_state(state);
  }

  #[test]
  fn test_dummy_driver_empty_tick() {
    let state = DummyDriver::new_state().unwrap();
    let (_submitter, mut handler) = DummyDriver::new(state).unwrap();

    let store = OpStore::new();

    // Tick without submitting any operations
    let completed = handler.try_tick(state, &store).unwrap();

    assert_eq!(completed.len(), 0);

    DummyDriver::drop_state(state);
  }
}
