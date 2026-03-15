//! Type-safe operation trait for bridging user types to backend execution.

use crate::backend::op::Op;

/// Implements `type Result` and `fn extract_result` for common io::Result patterns.
///
/// # Usage
///
/// ```ignore
/// impl TypedOp for MyOp {
///   impl_io_result!();           // io::Result<()>
///   impl_io_result!(i32);        // io::Result<i32>, returns res as i32
///   impl_io_result!(Resource);   // io::Result<Resource>, returns from_raw_fd(res)
///
///   fn into_op(&mut self) -> Op { ... }
/// }
/// ```
#[macro_export]
macro_rules! impl_io_result {
  // io::Result<()> - unit result
  () => {
    type Result = std::io::Result<()>;

    fn extract_result(self, res: isize) -> Self::Result {
      if res < 0 {
        Err(std::io::Error::from_raw_os_error((-res) as i32))
      } else {
        Ok(())
      }
    }
  };

  // io::Result<i32> - return res as i32
  (i32) => {
    type Result = std::io::Result<i32>;

    fn extract_result(self, res: isize) -> Self::Result {
      if res < 0 {
        Err(std::io::Error::from_raw_os_error((-res) as i32))
      } else {
        Ok(res as i32)
      }
    }
  };

  // io::Result<Resource> - return from_raw_fd(res)
  (Resource) => {
    type Result = std::io::Result<$crate::api::resource::Resource>;

    fn extract_result(self, res: isize) -> Self::Result {
      use std::os::fd::FromRawFd;
      if res < 0 {
        Err(std::io::Error::from_raw_os_error((-res) as i32))
      } else {
        // SAFETY: res is a valid file descriptor returned by the kernel
        Ok(unsafe { $crate::api::resource::Resource::from_raw_fd(res as i32) })
      }
    }
  };
}

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
