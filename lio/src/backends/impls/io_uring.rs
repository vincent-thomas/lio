//! `lio`-provided [`IoBackend`] impl for `io_uring`.

use lio_uring::{completion::CompletionQueue, submission::SubmissionQueue};

use crate::{
  backends::{IoBackend, OpCompleted, OpStore},
  operation::Operation,
};
use std::io;
use std::time::Duration;

/// io_uring backend for Linux.
///
/// This is the highest-performance backend, using Linux's io_uring interface
/// for truly asynchronous I/O with minimal syscall overhead.
///
/// # Example
///
/// ```rust,ignore
/// let mut backend = IoUring::default();
/// backend.init(1024)?;
///
/// backend.push(id, &op)?;
/// backend.flush()?;
///
/// let completions = backend.wait(&mut store)?;
/// ```
#[derive(Default)]
pub struct IoUring {
  sq: Option<SubmissionQueue>,
  cq: Option<CompletionQueue>,
  /// Reusable buffer for completed operations (avoids allocation per poll/wait).
  completed: Vec<OpCompleted>,
}

impl IoUring {
  /// Create a new uninitialized io_uring backend.
  ///
  /// Call [`init`](Self::init) before using.
  pub fn new() -> Self {
    Self::default()
  }

  #[inline]
  fn sq(&mut self) -> &mut SubmissionQueue {
    self.sq.as_mut().expect("IoUring not initialized - call init() first")
  }

  #[inline]
  fn cq(&mut self) -> &mut CompletionQueue {
    self.cq.as_mut().expect("IoUring not initialized - call init() first")
  }

  /// Poll for completions with optional timeout.
  ///
  /// - `timeout = None`: Block indefinitely
  /// - `timeout = Some(Duration::ZERO)`: Non-blocking poll
  /// - `timeout = Some(duration)`: Wait up to duration
  fn poll_inner(
    &mut self,
    timeout: Option<Duration>,
  ) -> io::Result<&[OpCompleted]> {
    self.completed.clear();
    let cq = self.cq();

    match timeout {
      None => {
        // Block indefinitely for first completion
        let first = cq.next()?;
        self
          .completed
          .push(OpCompleted::new(first.user_data(), first.result() as isize));
      }
      Some(d) if d.is_zero() => {
        // Non-blocking: check if anything is ready
        match cq.try_next()? {
          Some(op) => {
            self
              .completed
              .push(OpCompleted::new(op.user_data(), op.result() as isize));
          }
          None => return Ok(&[]),
        }
      }
      Some(d) => {
        // Wait with timeout
        match cq.next_timeout(d)? {
          Some(first) => {
            self
              .completed
              .push(OpCompleted::new(first.user_data(), first.result() as isize));
          }
          None => return Ok(&[]), // Timeout expired
        }
      }
    }

    // Drain any additional completions (non-blocking)
    while let Ok(Some(op)) = cq.try_next() {
      self
        .completed
        .push(OpCompleted::new(op.user_data(), op.result() as isize));
    }

    Ok(&self.completed)
  }
}

impl IoBackend for IoUring {
  fn init(&mut self, cap: usize) -> io::Result<()> {
    let (sq, cq) = lio_uring::with_capacity(cap as u32)?;
    self.sq = Some(sq);
    self.cq = Some(cq);
    // Pre-allocate completions buffer (reasonable batch size)
    self.completed = Vec::with_capacity(cap.min(256));
    Ok(())
  }

  fn push(&mut self, id: u64, op: &dyn Operation) -> io::Result<()> {
    let entry = op.create_entry();

    // Push to submission queue without syscall
    unsafe { self.sq().push(entry, id) }.map_err(|_| {
      io::Error::new(io::ErrorKind::WouldBlock, "submission queue full")
    })?;

    Ok(())
  }

  fn flush(&mut self) -> io::Result<usize> {
    // Submit all queued operations with a single syscall
    let submitted = self.sq().submit()?;
    Ok(submitted)
  }

  fn wait_timeout(
    &mut self,
    _store: &mut OpStore,
    timeout: Option<Duration>,
  ) -> io::Result<&[OpCompleted]> {
    self.poll_inner(timeout)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_init() {
    let mut backend = IoUring::new();
    backend.init(64).unwrap();
  }
}
