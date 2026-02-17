//! Dummy IoBackend for testing purposes
//!
//! This is a simple, synchronous implementation that stores operations in a queue
//! and completes them immediately when polled. Useful for testing without actual I/O.

use std::io;
use std::time::Duration;

use crate::{
  backends::{IoBackend, OpCompleted},
  op::Op,
};

/// A pending operation in the dummy backend
struct PendingOp {
  id: u64,
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
/// backend.push(id, op)?;
/// let completions = backend.poll()?;
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
    self.completed = Vec::with_capacity(cap.min(256));
    self.initialized = true;
    Ok(())
  }

  fn push(&mut self, id: u64, _op: Op) -> io::Result<()> {
    assert!(self.initialized, "DummyBackend not initialized");
    self.pending.push(PendingOp { id });
    Ok(())
  }

  fn flush(&mut self) -> io::Result<usize> {
    Ok(0)
  }

  fn wait_timeout(
    &mut self,
    _timeout: Option<Duration>,
  ) -> io::Result<&[OpCompleted]> {
    self.completed.clear();
    for op in self.pending.drain(..) {
      self.completed.push(OpCompleted::new(op.id, 0));
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
    let mut backend = DummyBackend::new();
    backend.init(64).unwrap();

    backend.push(1, Op::Nop).unwrap();
    backend.push(2, Op::Nop).unwrap();

    let completions = backend.wait_timeout(Some(Duration::ZERO)).unwrap();
    assert_eq!(completions.len(), 2);
  }
}
