//! Operation submission for I/O backends.
//!
//! This module defines the traits and types for submitting operations to backends.
//! Operations are registered in the [`OpStore`] before being submitted to the backend.

use std::sync::Arc;

use crate::{
  backends::{OpStore, SubmitErr},
  operation::Operation,
  registration::StoredOp,
};

/// Represents a completed I/O operation.
///
/// This type is returned by backend handlers when an operation completes,
/// containing the operation ID and its result.
#[derive(Debug)]
pub struct OpCompleted {
  /// The unique identifier of the completed operation.
  pub(crate) op_id: u64,

  /// Result of the operation:
  /// - `>= 0` on success (the return value, e.g., bytes transferred)
  /// - `< 0` on error (negative errno value)
  pub(crate) result: isize,
}

impl OpCompleted {
  /// Creates a new completed operation result.
  ///
  /// # Parameters
  ///
  /// - `op_id`: The unique ID of the operation
  /// - `result`: The operation result (non-negative for success, negative errno for error)
  pub fn new(op_id: u64, result: isize) -> Self {
    Self { op_id, result }
  }
}

/// Trait for backend-specific operation submission.
///
/// This trait is implemented by each backend to handle submitting operations
/// to the underlying I/O mechanism (io_uring, kqueue, IOCP, etc.).
pub trait IoSubmitter {
  /// Submits an operation to the backend.
  ///
  /// The caller guarantees that the operation is already registered in the [`OpStore`]
  /// with the given `id`.
  ///
  /// # Parameters
  ///
  /// - `state`: Type-erased pointer to the backend state
  /// - `id`: The [OpStore] key for op.
  /// - `op`: Operation to submit
  fn submit(
    &mut self,
    state: *const (),
    id: u64,
    op: &dyn Operation,
  ) -> Result<(), SubmitErr>;

  /// Notifies the backend that operations have been submitted. [`Handler::tick`](super::Handler::tick)'s must wake up.
  ///
  /// # Parameters
  ///
  /// - `state`: Type-erased pointer to the backend state
  fn notify(&mut self, state: *const ()) -> Result<(), SubmitErr>;
}

/// Type-erased submitter wrapper.
///
/// This wraps a backend-specific [`IoSubmitter`] implementation and provides
/// a unified interface for operation submission. It manages the connection
/// to the [`OpStore`] and backend state.
pub struct Submitter {
  sub: Box<dyn IoSubmitter>,
  store: Arc<OpStore>,
  state: *const (),
}

unsafe impl Send for Submitter {}

impl Submitter {
  /// Creates a new submitter wrapper.
  ///
  /// # Parameters
  ///
  /// - `sub`: The backend-specific submitter implementation
  /// - `store`: The operation store for tracking in-flight operations
  /// - `state`: Type-erased pointer to the backend state
  pub fn new(
    sub: Box<dyn IoSubmitter>,
    store: Arc<OpStore>,
    state: *const (),
  ) -> Self {
    Self { sub, store, state }
  }

  /// Submits an operation to the backend.
  ///
  /// # Returns
  ///
  /// - `Ok(u64)`: The unique operation ID
  /// - `Err(SubmitErr)`: Submission failed
  ///
  /// # Panics
  ///
  /// Panics if the backend returns `NotCompatible` after the operation was
  /// already inserted into the store. This indicates a bug in the backend
  /// selection logic.
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

  /// Notifies the backend to process submitted operations.
  ///
  /// Some backends batch operations and require an explicit notification
  /// to begin processing. This method should be called after one or more
  /// operations have been submitted.
  ///
  /// # Returns
  ///
  /// - `Ok(())`: Notification successful
  /// - `Err(SubmitErr)`: Notification failed
  pub fn notify(&mut self) -> Result<(), SubmitErr> {
    self.sub.notify(self.state)
  }
}
