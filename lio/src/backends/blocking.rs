//! Blocking I/O backend for operations incompatible with async polling.
//!
//! This backend handles operations that can't be submitted to epoll/kqueue/io_uring,
//! such as `openat`, `close`, `fsync`, etc. Operations are executed synchronously
//! in the submitter thread, with results collected by the handler thread.

use std::io;

use crate::backends::{
  IoDriver, IoHandler, IoSubmitter, OpCompleted, SubmitErr,
};
use crate::op::Operation;
use crate::store::OpStore;

/// Pending operation waiting to be executed
struct PendingOp {
  id: u64,
  result: isize,
}

/// Shared state between submitter and handler
pub(crate) struct BlockingState {}

pub struct BlockingSubmitter {
  completed_tx: crossbeam_channel::Sender<SubmitMsg>,
}

pub struct BlockingHandler {
  completed_rx: crossbeam_channel::Receiver<SubmitMsg>,
}

pub enum SubmitMsg {
  Op(PendingOp),
  Notify,
}

impl BlockingSubmitter {
  pub fn new(state: crossbeam_channel::Sender<SubmitMsg>) -> Self {
    Self { completed_tx: state }
  }
}

impl BlockingHandler {
  pub fn new(state: crossbeam_channel::Receiver<SubmitMsg>) -> Self {
    Self { completed_rx: state }
  }
}

impl IoSubmitter for BlockingSubmitter {
  fn submit(
    &mut self,
    _state: *const (),
    id: u64,
    op: &dyn Operation,
  ) -> Result<(), SubmitErr> {
    eprintln!("[BlockingSubmitter::submit] Executing operation id={}", id);
    // Execute the operation immediately in the submitter thread
    let result = op.run_blocking();

    eprintln!(
      "[BlockingSubmitter::submit] Operation id={} completed with result={}",
      id, result
    );

    // Store the result for the handler to pick up
    self.completed_tx.send(SubmitMsg::Op(PendingOp { id, result })).unwrap();

    Ok(())
  }

  fn notify(&mut self, _state: *const ()) -> Result<(), SubmitErr> {
    self
      .completed_tx
      .send(SubmitMsg::Notify)
      .map_err(|_| SubmitErr::DriverShutdown)
  }
}

impl IoHandler for BlockingHandler {
  fn tick(
    &mut self,
    _state: *const (),
    _store: &OpStore,
  ) -> io::Result<Vec<OpCompleted>> {
    let mut vec = Vec::new();
    let first = self.completed_rx.recv().map_err(|_| {
      io::Error::new(io::ErrorKind::BrokenPipe, "channel disconnected")
    })?;

    vec.push(first);

    while let Ok(pending) = self.completed_rx.try_recv() {
      vec.push(pending);
    }

    let result = vec
      .into_iter()
      .filter_map(|x| match x {
        SubmitMsg::Op(op) => Some(OpCompleted::new(op.id, op.result)),
        SubmitMsg::Notify => None,
      })
      .collect();

    Ok(result)
  }

  fn try_tick(
    &mut self,
    _state: *const (),
    _store: &OpStore,
  ) -> io::Result<Vec<OpCompleted>> {
    // Non-blocking: collect all available operations
    let mut completed = Vec::new();

    while let Ok(pending) = self.completed_rx.try_recv() {
      match pending {
        SubmitMsg::Notify => {}
        SubmitMsg::Op(op) => {
          completed.push(OpCompleted::new(op.id, op.result));
        }
      };
    }

    Ok(completed)
  }
}

pub struct BlockingBackend;

impl IoDriver for BlockingBackend {
  type Submitter = BlockingSubmitter;
  type Handler = BlockingHandler;

  fn new_state() -> io::Result<*const ()> {
    Ok(std::ptr::null())
  }

  fn drop_state(state: *const ()) {
    assert!(state.is_null());
  }

  fn new(_: *const ()) -> io::Result<(Self::Submitter, Self::Handler)> {
    let (sender, receiver) = crossbeam_channel::unbounded();
    Ok((BlockingSubmitter::new(sender), BlockingHandler::new(receiver)))
  }
}

#[cfg(test)]
test_io_driver!(BlockingBackend);

#[cfg(test)]
mod tests {
  use super::*;
  use crate::op::nop::Nop;
  use std::time::{Duration, Instant};

  // #[test]
  // fn test_blocking_handler_waits() {
  //   let state = BlockingBackend::new_state().unwrap();
  //   let (mut submitter, mut handler) = BlockingBackend::new(state).unwrap();
  //
  //   let store = OpStore::with_capacity(10);
  //
  //   // Start a thread that will submit an operation after a delay
  //   let state_ptr = state;
  //   thread::spawn(move || {
  //     thread::sleep(Duration::from_millis(100));
  //     submitter.submit(state_ptr, 1, &Nop).unwrap();
  //   });
  //
  //   // tick() should block until the operation completes
  //   let start = Instant::now();
  //   let result = handler.tick(state, &store).unwrap();
  //   let elapsed = start.elapsed();
  //
  //   // Should have waited for the operation
  //   assert!(elapsed >= Duration::from_millis(100));
  //   assert_eq!(result.len(), 1);
  //   assert_eq!(result[0].op_id, 1);
  //
  //   BlockingBackend::drop_state(state);
  // }

  #[test]
  fn test_try_tick_does_not_block() {
    let state = BlockingBackend::new_state().unwrap();
    let (_submitter, mut handler) = BlockingBackend::new(state).unwrap();

    let store = OpStore::with_capacity(10);

    // try_tick should return immediately even with no operations
    let start = Instant::now();
    let result = handler.try_tick(state, &store).unwrap();
    let elapsed = start.elapsed();

    // Should not have waited
    assert!(elapsed < Duration::from_millis(10));
    assert_eq!(result.len(), 0);

    BlockingBackend::drop_state(state);
  }

  #[test]
  fn test_tick_collects_multiple_operations() {
    let state = BlockingBackend::new_state().unwrap();
    let (mut submitter, mut handler) = BlockingBackend::new(state).unwrap();

    let store = OpStore::with_capacity(10);

    // Submit multiple operations before ticking
    submitter.submit(state, 1, &Nop).unwrap();
    submitter.submit(state, 2, &Nop).unwrap();
    submitter.submit(state, 3, &Nop).unwrap();

    // tick() should collect all available operations
    let result = handler.tick(state, &store).unwrap();

    assert_eq!(result.len(), 3);
    let ids: Vec<_> = result.iter().map(|op| op.op_id).collect();
    assert!(ids.contains(&1));
    assert!(ids.contains(&2));
    assert!(ids.contains(&3));

    BlockingBackend::drop_state(state);
  }

  // #[test]
  // fn test_notify_wakes_handler() {
  //   let state = BlockingBackend::new_state().unwrap();
  //   let (mut submitter, mut handler) = BlockingBackend::new(state).unwrap();
  //
  //   let store = OpStore::with_capacity(10);
  //
  //   let state_arc = unsafe { Arc::from_raw(state as *const BlockingState) };
  //   let state_clone = Arc::clone(&state_arc);
  //   let _ = Arc::into_raw(state_arc);
  //
  //   // Start handler in another thread
  //   let handler_thread = thread::spawn(move || {
  //     let start = Instant::now();
  //     let result = handler.tick(state, &store).unwrap();
  //     (start.elapsed(), result)
  //   });
  //
  //   thread::sleep(Duration::from_millis(50));
  //
  //   // Push directly to queue and notify
  //   state_clone.completed.push(PendingOp { id: 42, result: 0 });
  //   state_clone.coordinator.notify_one();
  //
  //   let (elapsed, result) = handler_thread.join().unwrap();
  //
  //   assert!(elapsed >= Duration::from_millis(50));
  //   assert_eq!(result.len(), 1);
  //   assert_eq!(result[0].op_id, 42);
  //
  //   BlockingBackend::drop_state(state);
  // }
}
