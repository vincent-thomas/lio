//! Type-safe operation trait for Op enum integration.
//!
//! This module provides the bridge between typed operations and the type-erased Op enum.
//! The [`TypedOp`] trait maintains compile-time type safety while allowing operations
//! to be executed through the unified Op enum interface.

use crate::op::Op;

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
///
/// # Example
///
/// ```rust,ignore
/// struct Read<T> { /* ... */ }
///
/// impl<T: BufLike> TypedOp for Read<T> {
///     type Result = BufResult<i32, T>;
///     
///     fn into_op(self) -> Op {
///         // Convert to type-erased Op
///     }
///     
///     fn extract_result(op_result: OpResult) -> Option<Self::Result> {
///         // Safely extract the correct result type
///     }
/// }
/// ```
pub trait TypedOp: Send + Sync + 'static {
  /// The typed result produced by this operation.
  type Result: Send + Sync;

  /// Convert this operation into the type-erased Op enum.
  ///
  /// This method consumes the operation and converts it to the unified
  /// representation used by backends for execution.
  fn into_op(&mut self) -> Op;

  /// Extract the typed result from an raw result.
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

pub struct ResultNotMatching;
