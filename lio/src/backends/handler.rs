//! Operation completion handling for I/O backends.
//!
//! This module defines the traits and types for processing completed I/O operations.
//! Handlers poll the backend for completion events and update the operation store.

use std::{io, sync::Arc};

use crate::backends::{OpCompleted, OpStore};

/// Trait for backend-specific completion event handling.
///
/// This trait is implemented by each backend to poll for and process completed
/// I/O operations. The handler retrieves completion events from the underlying
/// I/O mechanism and returns them to be processed by the runtime.
pub trait IoDriver {
  /// Attempts to poll for completed operations without blocking.
  ///
  /// This is a non-blocking poll that returns immediately if no completions
  /// are available.
  ///
  /// # Parameters
  ///
  /// - `state`: Type-erased pointer to the backend state
  /// - `store`: The operation store for looking up operations
  ///
  /// # Returns
  ///
  /// - `Ok(Vec<OpCompleted>)`: List of completed operations (may be empty)
  /// - `Err(io::Error)`: An error occurred while polling
  fn try_tick(
    &mut self,
    state: *const (),
    store: &OpStore,
  ) -> io::Result<Vec<OpCompleted>>;

  /// Polls for completed operations, blocking if necessary.
  ///
  /// This method waits for at least one operation to complete before returning.
  /// It's typically used by the runtime's event loop thread.
  ///
  /// # Parameters
  ///
  /// - `state`: Type-erased pointer to the backend state
  /// - `store`: The operation store for looking up operations
  ///
  /// # Returns
  ///
  /// - `Ok(Vec<OpCompleted>)`: List of completed operations (at least one)
  /// - `Err(io::Error)`: An error occurred while polling
  fn tick(
    &mut self,
    state: *const (),
    store: &OpStore,
  ) -> io::Result<Vec<OpCompleted>>;
}

/// Type-erased handler wrapper.
///
/// This wraps a backend-specific [`IoHandler`] implementation and provides
/// a unified interface for processing completions. It manages the connection
/// to the [`OpStore`] and backend state.
pub struct Driver {
  io: Box<dyn IoDriver>,
  state: *const (),
  store: Arc<OpStore>,
}

unsafe impl Send for Driver {}

impl Driver {
  /// Creates a new handler wrapper.
  ///
  /// # Parameters
  ///
  /// - `io`: The backend-specific handler implementation
  /// - `store`: The operation store for tracking in-flight operations
  /// - `state`: Type-erased pointer to the backend state
  pub fn new(
    io: Box<dyn IoDriver>,
    store: Arc<OpStore>,
    state: *const (),
  ) -> Self {
    Self { io, store, state }
  }

  /// Processes completed operations, blocking if necessary.
  ///
  /// This method:
  /// 1. Polls the backend for completed operations (blocks until at least one completes)
  /// 2. Updates each completed operation in the [`OpStore`] with its result
  /// 3. Wakes any tasks waiting on the completed operations
  ///
  /// This is the main event loop method used by the runtime's completion thread.
  ///
  /// # Returns
  ///
  /// - `Ok(())`: Successfully processed completions
  /// - `Err(io::Error)`: An error occurred while polling
  pub fn tick(&mut self) -> io::Result<()> {
    let result = self.io.tick(self.state, &self.store)?;

    for one in result {
      eprintln!(
        "[Handler::tick] Setting op_id={} as done with result={}",
        one.op_id, one.result
      );
      let exists =
        self.store.get_mut(one.op_id, |reg| reg.set_done(one.result));

      eprintln!(
        "[Handler::tick] Op {} exists in store: {:?}",
        one.op_id,
        exists.is_some()
      );
    }
    Ok(())
  }

  /// Processes completed operations without blocking.
  ///
  /// This is similar to [`tick`](Self::tick) but returns immediately if no
  /// completions are available. Used primarily in tests.
  ///
  /// # Returns
  ///
  /// - `Ok(())`: Successfully processed completions (may be zero)
  /// - `Err(io::Error)`: An error occurred while polling
  #[cfg(test)]
  pub fn try_tick(&mut self) -> io::Result<()> {
    let result = self.io.try_tick(self.state, &self.store)?;
    // assert!(!result.is_empty());

    for one in result {
      let _exists = self.store.get_mut(one.op_id, |reg| {
        reg.set_done(one.result);
      });
    }

    Ok(())
  }
}
