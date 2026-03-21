//! Type-safe operation trait for bridging user types to backend execution.

use crate::backend::op::Op;

/// Core trait for type-safe operations that can be converted to Op enum.
///
/// This trait provides a bridge between strongly-typed operations and the
/// type-erased [`Op`] enum used by backends. Each operation implements this
/// trait to maintain its type information throughout the execution pipeline.
///
/// # Type Safety
///
/// - The `Result` associated type is tied to the operation at compile time
/// - Result extraction is guaranteed to return the correct type
/// - No unsafe pointer casting needed
#[allow(clippy::wrong_self_convention)]
pub trait TypedOp: Send + Sync + 'static {
  /// The typed result produced by this operation.
  type Result: Send + Sync;

  /// Convert this operation into the type-erased Op enum.
  ///
  /// This method converts the operation to the unified
  /// representation used by backends for execution.
  fn into_op(&mut self) -> Op;

  /// Extract the typed result from a raw result.
  ///
  /// # Parameters
  ///
  /// - `op_result`: The raw result from executing the Op
  ///
  /// # Type Safety
  ///
  /// This method assumes that `self` contains the correct data that was
  /// used to create the Op, ensuring type-safe result extraction.
  fn extract_result(self, op_result: isize) -> Self::Result;
}

/// Result from extracting a stream item.
#[derive(Debug)]
pub enum StreamResult<T> {
  /// Stream yielded an item.
  Item(T),
  /// Stream is done (no more items).
  Done,
}

/// Trait for streaming operations that yield multiple items.
///
/// Unlike [`TypedOp`] which produces a single result, `StreamOp` can yield
/// multiple items over time. This is useful for operations like accepting
/// connections or watching file changes.
///
/// # Platform Differences
///
/// - **io_uring**: Uses multishot operations where a single submission can
///   yield multiple completions.
/// - **kqueue/epoll**: Each completion triggers a resubmission of the operation.
#[allow(clippy::wrong_self_convention)]
pub trait StreamOp: Send + Sync + 'static {
  /// The type of item yielded by each iteration.
  type Item: Send + Sync;

  /// Convert this operation into the type-erased Op enum.
  ///
  /// For streaming operations, this may be called multiple times
  /// (once per resubmission on non-multishot backends).
  fn into_op(&mut self) -> Op;

  /// Extract one item from a completion result.
  ///
  /// Returns `StreamResult::Item(item)` if the operation yielded an item,
  /// or `StreamResult::Done` if the stream is complete.
  ///
  /// # Parameters
  ///
  /// - `result`: The raw result from the backend completion
  fn extract_item(&mut self, result: isize) -> StreamResult<Self::Item>;
}
