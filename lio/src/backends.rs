use crate::{OperationProgress, driver::OpStore, op::Operation};

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
mod threading;
#[allow(unused_imports)]
pub use threading::*;

/// Trait defining the interface for I/O operation execution backends.
///
/// Backends handle the lifecycle of operations from submission through completion,
/// managing resources and notifying the driver when operations finish.
///
/// # Contract
///
/// Implementations must:
/// - Eventually complete all submitted operations
/// - Have operation state synced with [OpStore].
/// - Deliver notifications to wakers or callbacks when operations complete
pub trait IoBackend: Send + Sync {
  /// Processes completed operations and advances the backend's internal state.
  ///
  /// `store` is the operation store containing all submitted operations.
  ///  This method is only allowed to block/wait-for-completion if `can_block`
  ///  is `true`.
  ///
  /// # Contract
  ///
  /// For each completed operation, implementations must:
  /// 1. Retrieve the operation result
  /// 2. Call `OpStore::get_mut()` to mark it as done via `set_done()`
  /// 3. Handle any extracted notifications (wakers or callbacks)
  /// 4. Clean up backend-specific resources
  fn tick(&self, store: &OpStore, can_wait: bool);

  /// Submits an operation to the backend for execution.
  ///
  /// `op` is the operation to execute and `store` is the operation store where
  /// in-progress operations are tracked.
  ///
  /// # Returns
  ///
  /// Returns an `OperationProgress` indicating how the operation will be handled.
  ///
  /// # Contract
  ///
  /// Implementations must:
  /// - Either complete the operation immediately or arrange for it to complete asynchronously
  /// - If completing asynchronously, insert the operation into the `OpStore` with a unique ID
  /// - Return an appropriate `OperationProgress` matching the chosen completion path
  fn submit<O>(&self, op: O, store: &OpStore) -> OperationProgress<O>
  where
    O: Operation + Sized;

  /// Wakes up a potentially blocking `tick()` call.
  ///
  /// # Contract
  ///
  /// Implementations must cause any concurrent `tick()` call with `can_wait = true`
  /// to return, even if no operations have completed.
  fn notify(&self);
}
