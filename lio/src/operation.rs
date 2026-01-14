//! Core traits and types for I/O operations.
//!
//! This module defines the fundamental abstraction for I/O operations in lio. All I/O
//! operations (read, write, accept, etc.) implement the [`Operation`] trait, which allows
//! them to be executed either asynchronously through backend mechanisms (io_uring, kqueue,
//! IOCP) or synchronously in blocking mode.
//!
//! # Core Traits
//!
//! - [`Operation`]: The base trait that all I/O operations must implement
//! - [`OperationExt`]: Extension trait that associates a typed result with an operation
//!
//! # Operation Metadata
//!
//! The [`OpMeta`] type provides metadata about operations using a compact bitflags
//! representation. This metadata describes:
//! - **Capabilities**: What resource type the operation uses (file descriptor, timer, or none)
//! - **Behavior**: Whether the operation needs to run before notification
//! - **I/O Direction**: Whether the operation reads or writes (for FD operations)
//!
//! # Examples
//!
//! Operations are typically not used directly by end users. Instead, they're used through
//! the high-level API:
//!
//! ```rust,ignore
//! use lio::api::resource::Resource;
//!
//! lio::init();
//!
//! async fn example(fd: Resource) -> std::io::Result<()> {
//!     let buffer = vec![0u8; 1024];
//!
//!     // read() creates a Read operation internally
//!     let (buffer, bytes_read) = lio::read(fd, buffer).await?;
//!
//!     Ok(())
//! }
//!
//! lio::exit();
//! ```
//!
//! # Implementing Custom Operations
//!
//! The [`Operation`] trait is sealed and can only be implemented within lio. This ensures
//! that all operations are properly integrated with the runtime and backends.

trait Sealed {}

/// Seal implementation to prevent external implementations of [`Operation`].
///
/// This ensures that all operations are properly integrated with lio's runtime
/// and prevents undefined behavior from improperly implemented operations.
impl<O: Operation> Sealed for O {}

/// Core trait for I/O operations.
///
/// This trait represents a command that can be executed either asynchronously through
/// platform-specific mechanisms (io_uring on Linux, kqueue on BSD/macOS, IOCP on Windows)
/// or synchronously in blocking mode.
///
/// # Sealed Trait
///
/// This trait is sealed and cannot be implemented outside of lio. All operation
/// implementations are provided by the library. This API choice is not stable
/// and may change.
///
/// # Result Handling
///
/// Operation results follow Unix conventions:
/// - `>= 0`: Success (the result value, e.g., bytes transferred)
/// - `< 0`: Error (negative errno value)
pub trait Operation: Sealed + Send + Sync {
  /// Processes the operation result and returns a pointer to the typed result.
  ///
  /// This method is called exactly once when an operation completes. It converts
  /// the raw syscall return value into a typed result appropriate for the operation.
  ///
  /// # Parameters
  ///
  /// - `ret`: The raw syscall return value (>= 0 for success, < 0 for -errno)
  ///
  /// # Returns
  ///
  /// A type-erased pointer to the operation's result. The pointer is valid until
  /// the operation is dropped.
  ///
  /// # Implementation Requirements
  ///
  /// The returned pointer must point to a valid result of type `Self::Result`
  /// (from [`OperationExt`]) and remain valid for the lifetime of the operation.
  fn result(&mut self, ret: isize) -> *const ();

  /// Creates an io_uring submission queue entry for this operation.
  ///
  /// This method is only available on Linux and is used by the io_uring backend
  /// to submit operations asynchronously.
  ///
  /// # Returns
  ///
  /// An io_uring SQE (submission queue entry) configured for this operation.
  #[cfg(linux)]
  fn create_entry(&self) -> lio_uring::submission::Entry;

  /// Returns metadata about this operation.
  ///
  /// The metadata describes the operation's capabilities, behavior, and I/O direction.
  /// See [`OpMeta`] for details.
  #[cfg(unix)]
  fn meta(&self) -> OpMeta;

  /// Returns the capability value for operations that use file descriptors or timers.
  ///
  /// This method is used by backends that need to track capabilities separately.
  /// Most operations don't need to override this.
  ///
  /// # Returns
  ///
  /// The raw file descriptor or timer ID.
  ///
  /// # Panics
  ///
  /// Panics if:
  /// - The operation has no capability ([`OpMeta::CAP_NONE`])
  /// - The operation has a capability but doesn't override this method
  #[cfg(unix)]
  fn cap(&self) -> i32 {
    let meta: OpMeta = self.meta();
    if meta.is_cap_none() {
      panic!(
        "lio: Operation::cap default impl
internal library caller error: operation has no cap."
      );
    } else {
      panic!(
        "lio: Operation::cap default impl
implemntee error: operation has cap, but cap fn isn't defined."
      );
    }
  }

  /// Executes the operation synchronously in blocking mode.
  ///
  /// This performs the syscall directly on the calling thread, blocking until
  /// the operation completes.
  ///
  /// # Returns
  ///
  /// The raw syscall return value:
  /// - `>= 0` on success (the result value, e.g., bytes read/written)
  /// - `< 0` on error (negative errno value)
  fn run_blocking(&self) -> isize;
}

/// Extension trait that associates a typed result with an operation.
///
/// This trait is implemented alongside [`Operation`] to provide type-safe access
/// to operation results. While [`Operation::result`] returns a type-erased pointer,
/// this trait's `Result` type describes what that pointer actually points to.
///
/// # Example
///
/// ```rust,ignore
/// use lio::operation::{Operation, OperationExt};
///
/// struct Read { /* ... */ }
///
/// impl OperationExt for Read {
///     type Result = io::Result<(Vec<u8>, usize)>;
/// }
/// ```
pub trait OperationExt: Operation {
  /// The typed result of this operation.
  ///
  /// This is typically an `io::Result<T>` where `T` is operation-specific,
  /// such as `(Vec<u8>, usize)` for read operations or `()` for operations
  /// that don't return a value.
  type Result;
}

/// Metadata describing an operation's capabilities and behavior.
///
/// `OpMeta` uses a compact bitflags representation to encode information about
/// an operation that backends use for scheduling and execution. The metadata
/// is stored in a single byte with the following bit layout:
///
/// ```text
/// Bit 7: CAP_FD      - Operation uses a file descriptor
/// Bit 6: CAP_TIMER   - Operation uses a timer
/// Bit 3: BEH_NEEDS_RUN - Operation needs to run before notification
/// Bit 1: FD_WRITE    - Operation waits for write readiness
/// Bit 0: FD_READ     - Operation waits for read readiness
/// ```
///
/// # Capabilities
///
/// Operations fall into three capability categories:
/// - **None** ([`CAP_NONE`](Self::CAP_NONE)): No special capability (e.g., nop)
/// - **File Descriptor** ([`CAP_FD`](Self::CAP_FD)): Uses a file descriptor (e.g., read, write, accept)
/// - **Timer** ([`CAP_TIMER`](Self::CAP_TIMER)): Uses a timer (e.g., timeout, sleep)
///
/// An operation cannot have both `CAP_FD` and `CAP_TIMER` set simultaneously.
///
/// # Behavior Flags
///
/// - [`BEH_NEEDS_RUN`](Self::BEH_NEEDS_RUN): Operation must execute before backend notification
///
/// # I/O Direction
///
/// For file descriptor operations, the direction flags indicate readiness:
/// - [`FD_READ`](Self::FD_READ): Operation waits for read readiness
/// - [`FD_WRITE`](Self::FD_WRITE): Operation waits for write readiness
///
/// # Examples
///
/// ```rust,ignore
/// use lio::operation::OpMeta;
///
/// // A read operation uses FD and waits for read readiness
/// let read_meta = OpMeta::CAP_FD | OpMeta::FD_READ;
/// assert!(read_meta.is_cap_fd());
/// assert!(read_meta.is_fd_readable());
///
/// // A write operation uses FD and waits for write readiness
/// let write_meta = OpMeta::CAP_FD | OpMeta::FD_WRITE;
/// assert!(write_meta.is_cap_fd());
/// assert!(write_meta.is_fd_writable());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpMeta(u8);

impl From<OpMeta> for u8 {
  fn from(val: OpMeta) -> Self {
    val.0
  }
}

impl std::ops::BitOr for OpMeta {
  type Output = Self;

  fn bitor(self, rhs: Self) -> Self::Output {
    Self(self.0 | rhs.0)
  }
}

impl OpMeta {
  /// No capability - operation doesn't use a file descriptor or timer.
  pub const CAP_NONE: Self = Self(0);

  /// Operation uses a file descriptor.
  ///
  /// Cannot be combined with [`CAP_TIMER`](Self::CAP_TIMER).
  pub const CAP_FD: Self = Self(1 << 7);

  /// Operation uses a timer.
  ///
  /// Cannot be combined with [`CAP_FD`](Self::CAP_FD).
  pub const CAP_TIMER: Self = Self(1 << 6);

  /// Operation needs to run before backend notification.
  ///
  /// Only valid for file descriptor operations.
  pub const BEH_NEEDS_RUN: Self = Self(1 << 3);

  /// Operation waits for write readiness on a file descriptor.
  ///
  /// Only valid for file descriptor operations.
  pub const FD_WRITE: Self = Self(1 << 1);

  /// Operation waits for read readiness on a file descriptor.
  ///
  /// Only valid for file descriptor operations.
  pub const FD_READ: Self = Self(1 << 0);

  /// Checks if the operation has no capability.
  ///
  /// # Returns
  ///
  /// `true` if neither [`CAP_FD`](Self::CAP_FD) nor [`CAP_TIMER`](Self::CAP_TIMER) is set.
  pub const fn is_cap_none(self) -> bool {
    // Check if neither CAP_FD nor CAP_TIMER bits are set
    (self.0 & (Self::CAP_FD.0 | Self::CAP_TIMER.0)) == 0
  }

  /// Checks if the operation uses a file descriptor.
  ///
  /// # Returns
  ///
  /// `true` if [`CAP_FD`](Self::CAP_FD) is set.
  ///
  /// # Panics
  ///
  /// Panics in debug mode if both `CAP_FD` and `CAP_TIMER` are set (invalid state).
  pub const fn is_cap_fd(self) -> bool {
    // Check if CAP_FD bit is set
    let has_cap_fd = (self.0 & Self::CAP_FD.0) != 0;
    if has_cap_fd {
      assert!(
        (self.0 & Self::CAP_TIMER.0) == 0,
        "Operation cannot have both CAP_FD and CAP_TIMER"
      );
    }
    has_cap_fd
  }

  /// Checks if the operation uses a timer.
  ///
  /// # Returns
  ///
  /// `true` if [`CAP_TIMER`](Self::CAP_TIMER) is set.
  ///
  /// # Panics
  ///
  /// Panics in debug mode if both `CAP_TIMER` and `CAP_FD` are set (invalid state).
  pub const fn is_cap_timer(self) -> bool {
    // Check if CAP_TIMER bit is set
    let has_cap_timer = (self.0 & Self::CAP_TIMER.0) != 0;
    if has_cap_timer {
      assert!(
        (self.0 & Self::CAP_FD.0) == 0,
        "Operation cannot have both CAP_FD and CAP_TIMER"
      );
    }
    has_cap_timer
  }

  /// Checks if the operation needs to run before backend notification.
  ///
  /// This flag indicates that the operation must execute its blocking code
  /// before the backend can notify about readiness.
  ///
  /// # Returns
  ///
  /// `true` if [`BEH_NEEDS_RUN`](Self::BEH_NEEDS_RUN) is set.
  ///
  /// # Panics
  ///
  /// Panics if the operation doesn't use a file descriptor ([`CAP_FD`](Self::CAP_FD) not set).
  pub const fn is_beh_run_before_noti(self) -> bool {
    assert!(self.0 & Self::CAP_FD.0 != 0);
    self.0 & Self::BEH_NEEDS_RUN.0 != 0
  }

  /// Checks if the operation waits for read readiness.
  ///
  /// # Returns
  ///
  /// `true` if [`FD_READ`](Self::FD_READ) is set.
  ///
  /// # Panics
  ///
  /// Panics if the operation doesn't use a file descriptor ([`CAP_FD`](Self::CAP_FD) not set).
  pub const fn is_fd_readable(self) -> bool {
    assert!(self.0 & Self::CAP_FD.0 != 0);
    self.0 & Self::FD_READ.0 != 0
  }

  /// Checks if the operation waits for write readiness.
  ///
  /// # Returns
  ///
  /// `true` if [`FD_WRITE`](Self::FD_WRITE) is set.
  ///
  /// # Panics
  ///
  /// Panics if the operation doesn't use a file descriptor ([`CAP_FD`](Self::CAP_FD) not set).
  pub const fn is_fd_writable(self) -> bool {
    assert!(self.0 & Self::CAP_FD.0 != 0);
    self.0 & Self::FD_WRITE.0 != 0
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_op_meta_cap_none() {
    let meta = OpMeta::CAP_NONE;
    assert!(meta.is_cap_none());
    assert!(!meta.is_cap_fd());
    assert!(!meta.is_cap_timer());
  }

  #[test]
  fn test_op_meta_cap_fd() {
    let meta = OpMeta::CAP_FD;
    assert!(!meta.is_cap_none());
    assert!(meta.is_cap_fd());
    assert!(!meta.is_cap_timer());
  }

  #[test]
  fn test_op_meta_cap_timer() {
    let meta = OpMeta::CAP_TIMER;
    assert!(!meta.is_cap_none());
    assert!(!meta.is_cap_fd());
    assert!(meta.is_cap_timer());
  }

  #[test]
  fn test_op_meta_fd_read() {
    let meta = OpMeta::CAP_FD | OpMeta::FD_READ;
    assert!(meta.is_fd_readable());
    assert!(!meta.is_fd_writable());
  }

  #[test]
  fn test_op_meta_fd_write() {
    let meta = OpMeta::CAP_FD | OpMeta::FD_WRITE;
    assert!(!meta.is_fd_readable());
    assert!(meta.is_fd_writable());
  }

  #[test]
  fn test_op_meta_size() {
    // Ensure OpMeta fits in a single byte
    assert_eq!(std::mem::size_of::<OpMeta>(), 1);
  }

  #[test]
  fn test_op_meta_values_unique() {
    // Ensure all variants have unique bit patterns
    let cap_none: u8 = OpMeta::CAP_NONE.into();
    let cap_fd: u8 = OpMeta::CAP_FD.into();
    let cap_timer: u8 = OpMeta::CAP_TIMER.into();
    let fd_read: u8 = (OpMeta::CAP_FD | OpMeta::FD_READ).into();
    let fd_write: u8 = (OpMeta::CAP_FD | OpMeta::FD_WRITE).into();

    let values = [cap_none, cap_fd, cap_timer, fd_read, fd_write];
    for (i, &val1) in values.iter().enumerate() {
      for &val2 in values.iter().skip(i + 1) {
        assert_ne!(val1, val2, "OpMeta variants must have unique values");
      }
    }
  }

  #[test]
  #[should_panic]
  fn test_op_meta_readable_panics_on_non_fd() {
    let meta = OpMeta::CAP_TIMER;
    let _ = meta.is_fd_readable(); // Should panic
  }

  #[test]
  #[should_panic]
  fn test_op_meta_writable_panics_on_non_fd() {
    let meta = OpMeta::CAP_NONE;
    let _ = meta.is_fd_writable(); // Should panic
  }
}
