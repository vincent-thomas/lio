//! Blocking I/O backend for operations incompatible with async polling.
//!
//! This backend handles operations that can't be submitted to epoll/kqueue/io_uring,
//! such as `openat`, `close`, `fsync`, etc. Operations are executed synchronously
//! in the submitter thread, with results collected by the handler thread.

use std::io;
use std::sync::Arc;

use crossbeam_queue::SegQueue;

use crate::backends::{
  IoDriver, IoHandler, IoSubmitter, OpCompleted, SubmitErr,
};
use crate::blocking_queue::BlockingCoordinator;
use crate::op::Operation;
use crate::store::OpStore;

/// Pending operation waiting to be executed
struct PendingOp {
  id: u64,
  result: isize,
}

/// Shared state between submitter and handler
pub(crate) struct BlockingState {
  completed: SegQueue<PendingOp>,
  coordinator: BlockingCoordinator,
}

pub struct BlockingSubmitter {
  state: Arc<BlockingState>,
}

pub struct BlockingHandler {
  state: Arc<BlockingState>,
}

impl BlockingSubmitter {
  pub fn new(state: Arc<BlockingState>) -> Self {
    Self { state }
  }
}

impl BlockingHandler {
  pub fn new(state: Arc<BlockingState>) -> Self {
    Self { state }
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

    eprintln!("[BlockingSubmitter::submit] Operation id={} completed with result={}", id, result);

    // Store the result for the handler to pick up
    self.state.completed.push(PendingOp { id, result });

    eprintln!("[BlockingSubmitter::submit] Pushed to completed queue, notifying handler");

    // Notify the handler that a new result is available
    self.state.coordinator.notify_one();

    eprintln!("[BlockingSubmitter::submit] Handler notified");

    Ok(())
  }

  fn notify(&mut self, _state: *const ()) -> Result<(), SubmitErr> {
    // Wake up the handler in case it's waiting
    self.state.coordinator.notify_one();
    Ok(())
  }
}

impl IoHandler for BlockingHandler {
  fn tick(
    &mut self,
    _state: *const (),
    _store: &OpStore,
  ) -> io::Result<Vec<OpCompleted>> {
    eprintln!("[BlockingHandler::tick] Waiting for operations...");
    // Wait for at least one operation to complete
    let first =
      self.state.coordinator.blocking_pop(|| self.state.completed.pop());

    eprintln!("[BlockingHandler::tick] Got first operation: id={}, result={}", first.id, first.result);

    let mut completed = vec![OpCompleted::new(first.id, first.result)];

    // Collect any additional completed operations without blocking
    while let Some(pending) = self.state.completed.pop() {
      eprintln!("[BlockingHandler::tick] Got additional operation: id={}, result={}", pending.id, pending.result);
      completed.push(OpCompleted::new(pending.id, pending.result));
    }

    eprintln!("[BlockingHandler::tick] Returning {} completed operations", completed.len());
    Ok(completed)
  }

  fn try_tick(
    &mut self,
    _state: *const (),
    _store: &OpStore,
  ) -> io::Result<Vec<OpCompleted>> {
    // Non-blocking: collect all available operations
    let mut completed = Vec::new();

    while let Some(pending) = self.state.completed.pop() {
      completed.push(OpCompleted::new(pending.id, pending.result));
    }

    Ok(completed)
  }
}

pub struct BlockingBackend;

impl IoDriver for BlockingBackend {
  type Submitter = BlockingSubmitter;
  type Handler = BlockingHandler;

  fn new_state() -> io::Result<*const ()> {
    let state = Arc::new(BlockingState {
      completed: SegQueue::new(),
      coordinator: BlockingCoordinator::new(),
    });

    Ok(Arc::into_raw(state) as *const ())
  }

  fn drop_state(state: *const ()) {
    let _ = unsafe { Arc::from_raw(state as *const BlockingState) };
  }

  fn new(state: *const ()) -> io::Result<(Self::Submitter, Self::Handler)> {
    // Reconstruct Arc temporarily to clone it
    let state_arc = unsafe { Arc::from_raw(state as *const BlockingState) };
    let state_clone = Arc::clone(&state_arc);
    // Prevent dropping the original
    let _ = Arc::into_raw(state_arc);

    Ok((
      BlockingSubmitter::new(state_clone.clone()),
      BlockingHandler::new(state_clone),
    ))
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::op::nop::Nop;
  use std::sync::Arc;
  use std::thread;
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
