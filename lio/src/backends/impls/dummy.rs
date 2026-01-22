//! Dummy IoBackend for testing purposes
//!
//! This is a simple, synchronous implementation that stores operations in a queue
//! and completes them immediately when polled. Useful for testing without actual I/O.

use std::io;
use std::time::Duration;

use crate::{
  backends::{IoBackend, OpCompleted, OpStore},
  operation::Operation,
};

/// A pending operation in the dummy backend
struct PendingOp {
  id: u64,
  result: i32,
}

/// Dummy backend for testing.
///
/// All operations complete immediately with success (result = 0).
/// Useful for testing the runtime without actual I/O.
///
/// # Example
///
/// ```rust,ignore
/// let mut backend = DummyBackend::default();
/// backend.init(64)?;
///
/// backend.push(id, &op)?;
/// let completions = backend.poll(&mut store)?;
/// ```
#[derive(Default)]
pub struct DummyBackend {
  pending: Vec<PendingOp>,
  /// Reusable buffer for completed operations (avoids allocation per poll/wait).
  completed: Vec<OpCompleted>,
  initialized: bool,
}

impl DummyBackend {
  pub fn new() -> Self {
    Self::default()
  }
}

impl IoBackend for DummyBackend {
  fn init(&mut self, cap: usize) -> io::Result<()> {
    self.pending = Vec::with_capacity(cap);
    self.completed = Vec::with_capacity(cap.min(256)); // Reusable completions buffer
    self.initialized = true;
    Ok(())
  }

  fn push(&mut self, id: u64, _op: &dyn Operation) -> io::Result<()> {
    assert!(self.initialized, "DummyBackend not initialized");
    // For dummy backend, all operations succeed immediately with result 0
    self.pending.push(PendingOp { id, result: 0 });
    Ok(())
  }

  fn flush(&mut self) -> io::Result<usize> {
    // Dummy backend doesn't need flushing
    Ok(0)
  }

  fn wait_timeout(
    &mut self,
    _store: &mut OpStore,
    _timeout: Option<Duration>,
  ) -> io::Result<&[OpCompleted]> {
    self.completed.clear();
    // Return all pending operations immediately (dummy doesn't actually wait)
    for op in self.pending.drain(..) {
      self.completed.push(OpCompleted::new(op.id, op.result as isize));
    }
    Ok(&self.completed)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_init() {
    let mut backend = DummyBackend::new();
    backend.init(64).unwrap();
  }

  #[test]
  fn test_push_and_poll() {
    use crate::api::ops::Nop;

    let mut backend = DummyBackend::new();
    backend.init(64).unwrap();

    let mut store = OpStore::with_capacity(64);

    // Push a nop operation
    backend.push(1, &Nop).unwrap();
    backend.push(2, &Nop).unwrap();

    // Poll should return both completions
    let completions = backend.wait_timeout(&mut store, Some(Duration::ZERO)).unwrap();
    assert_eq!(completions.len(), 2);
  }
}
