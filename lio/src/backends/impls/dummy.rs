//! Dummy IoDriver for testing purposes
//!
//! This is a simple, synchronous implementation that stores operations in a queue
//! and completes them immediately when polled. Useful for testing Worker behavior
//! without dealing with actual I/O operations.

use std::{io, sync::Arc};

use crate::{
  backends::{
    IoBackend, IoDriver, IoSubmitter, OpCompleted, OpStore, SubmitErr,
  },
  operation::Operation,
};

#[derive(Clone)]
pub struct DummyState {
  pending_tx: crossbeam_channel::Sender<(u64, i32)>,
  pending_rx: crossbeam_channel::Receiver<(u64, i32)>,
}

impl DummyState {
  fn new() -> Self {
    let (pending_tx, pending_rx) = crossbeam_channel::unbounded();
    Self { pending_tx, pending_rx }
  }
}

pub struct DummyDriver;

impl IoBackend for DummyDriver {
  type Submitter = DummySubmitter;
  type Driver = DummyHandler;
  type State = DummyState;

  fn new_state() -> io::Result<Self::State> {
    Ok(DummyState::new())
  }

  fn new(
    state: Arc<Self::State>,
  ) -> io::Result<(Self::Submitter, Self::Driver)> {
    // let state_ref = unsafe { &*(state as *const DummyState) };
    Ok((DummySubmitter { state: state.clone() }, DummyHandler { state }))
  }
}

pub struct DummySubmitter {
  state: Arc<DummyState>,
}

impl IoSubmitter for DummySubmitter {
  fn submit(&mut self, id: u64, _op: &dyn Operation) -> Result<(), SubmitErr> {
    // For dummy driver, we just queue the operation with a successful result
    let _ = self.state.pending_tx.send((id, 0)); // 0 represents success
    Ok(())
  }

  fn notify(&mut self) -> Result<(), SubmitErr> {
    // Nothing to do - channel already handles notification
    Ok(())
  }
}

pub struct DummyHandler {
  state: Arc<DummyState>,
}

impl IoDriver for DummyHandler {
  fn try_tick(&mut self, _store: &OpStore) -> io::Result<Vec<OpCompleted>> {
    let mut completed = Vec::new();

    // Drain all available operations without blocking
    while let Ok((op_id, result)) = self.state.pending_rx.try_recv() {
      completed.push(OpCompleted::new(op_id, result as isize));
    }

    Ok(completed)
  }

  fn tick(&mut self, _store: &OpStore) -> io::Result<Vec<OpCompleted>> {
    // Block with timeout to allow handler_loop to check shutdown signals
    let first_op =
      self.state.pending_rx.recv_timeout(std::time::Duration::from_millis(100));

    let mut completed = Vec::new();

    // If we got an operation, add it to completed
    if let Ok((op_id, result)) = first_op {
      completed.push(OpCompleted::new(op_id, result as isize));
    }

    // Collect any additional operations that arrived
    while let Ok((op_id, result)) = self.state.pending_rx.try_recv() {
      completed.push(OpCompleted::new(op_id, result as isize));
    }

    Ok(completed)
  }
}

// #[cfg(test)]
// mod tests {
//   use super::*;
//   use crate::{op::nop::Nop, registration::StoredOp};
//
//   #[test]
//   fn test_dummy_driver_basic() {
//     let state = DummyDriver::new_state().unwrap();
//     let (mut submitter, mut handler) = DummyDriver::new(state).unwrap();
//
//     let store = OpStore::new();
//     let op = StoredOp::new_waker(Nop);
//     let id = store.insert(op);
//
//     // Submit operation
//     submitter.submit(state, id, &Nop).unwrap();
//
//     // Tick handler to complete operation
//     let completed = handler.try_tick(state, &store).unwrap();
//
//     assert_eq!(completed.len(), 1);
//     assert_eq!(completed[0].op_id, id);
//     assert!(completed[0].result >= 0);
//
//     DummyDriver::drop_state(state);
//   }
//
//   #[test]
//   fn test_dummy_driver_multiple_ops() {
//     let state = DummyDriver::new_state().unwrap();
//     let (mut submitter, mut handler) = DummyDriver::new(state).unwrap();
//
//     let store = OpStore::new();
//     let mut ids = Vec::new();
//
//     // Submit 10 operations
//     for _ in 0..10 {
//       let op = StoredOp::new_waker(Nop);
//       let id = store.insert(op);
//       submitter.submit(state, id, &Nop).unwrap();
//       ids.push(id);
//     }
//
//     // Tick handler to complete all operations
//     let completed = handler.try_tick(state, &store).unwrap();
//
//     assert_eq!(completed.len(), 10);
//     for (i, comp) in completed.iter().enumerate() {
//       assert_eq!(comp.op_id, ids[i]);
//       assert!(comp.result >= 0);
//     }
//
//     DummyDriver::drop_state(state);
//   }
//
//   #[test]
//   fn test_dummy_driver_empty_tick() {
//     let state = DummyDriver::new_state().unwrap();
//     let (_submitter, mut handler) = DummyDriver::new(state).unwrap();
//
//     let store = OpStore::new();
//
//     // Tick without submitting any operations
//     let completed = handler.try_tick(state, &store).unwrap();
//
//     assert_eq!(completed.len(), 0);
//
//     DummyDriver::drop_state(state);
//   }
// }
