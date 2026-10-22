//! Type-safe operation traits for bridging user types to backend execution.

use crate::api::resource::CloseFn;
use crate::backend::op::Op;
use std::time::Duration;

/// Completion metadata routed from the backend to one `OpModel`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Completion {
  pub result: isize,
  pub flags: CompletionFlags,
  /// How a resource carried by `result` is closed: the producing backend's
  /// close function.
  resource_close: ResourceClose,
}

impl Completion {
  /// A completion whose resources (if any) are native OS descriptors/handles.
  pub const fn new(result: isize) -> Self {
    Self::with_flags(result, CompletionFlags::empty())
  }

  pub const fn with_flags(result: isize, flags: CompletionFlags) -> Self {
    Self {
      result,
      flags,
      resource_close: ResourceClose(crate::backend::native::close),
    }
  }

  /// Sets how a resource carried by this completion is closed.
  pub(crate) const fn with_resource_close(mut self, close: CloseFn) -> Self {
    self.resource_close = ResourceClose(close);
    self
  }

  /// How a resource carried by this completion is closed.
  pub(crate) const fn resource_close(&self) -> CloseFn {
    self.resource_close.0
  }
  /// The error a failed completion (a negative `result`) reports.
  ///
  /// A result that is no OS error code, because negating it overflows or it
  /// does not fit an `i32`, comes from a broken backend; it is reported as
  /// such rather than wrapped into some other code.
  pub(crate) fn os_error(&self) -> std::io::Error {
    match self.result.checked_neg().and_then(|code| i32::try_from(code).ok()) {
      Some(code) => std::io::Error::from_raw_os_error(code),
      None => std::io::Error::other(format!(
        "backend completed with an invalid error code {}",
        self.result
      )),
    }
  }
}

/// A [`CloseFn`] compared by address.
#[derive(Clone, Copy)]
struct ResourceClose(CloseFn);

impl std::fmt::Debug for ResourceClose {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str("ResourceClose")
  }
}

impl PartialEq for ResourceClose {
  fn eq(&self, other: &Self) -> bool {
    std::ptr::fn_addr_eq(self.0, other.0)
  }
}

impl Eq for ResourceClose {}

/// Completion flags understood by higher-level operation models.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompletionFlags(u32);

impl CompletionFlags {
  pub const MORE: Self = Self(1 << 0);
  pub const CANCELLED: Self = Self(1 << 1);
  pub const TERMINAL: Self = Self(1 << 2);
  pub const TIMEOUT: Self = Self(1 << 3);
  pub const TIMER: Self = Self(1 << 4);

  pub const fn empty() -> Self {
    Self(0)
  }

  pub const fn contains(self, other: Self) -> bool {
    (self.0 & other.0) == other.0
  }
}

impl std::ops::BitOr for CompletionFlags {
  type Output = Self;

  fn bitor(self, rhs: Self) -> Self::Output {
    Self(self.0 | rhs.0)
  }
}

/// Result of one completion.
pub enum OpResult<T> {
  /// Model doesn't have an [`Item`](OpModel::Item) to return. The caller should
  /// run [`action`](OpModel::action) again.
  Again,
  /// Model returns an [`Item`](OpModel::Item), while also wanting
  /// [`action`](OpModel::action) to be called again.
  ///
  /// Models are required to implement [`StreamOpModel`] to return this.
  Yield(T),
  /// Return a final [`Item`](OpModel::Item), the model is now done and should
  /// be dropped.
  Done(T),
}

/// One action requested by an [`OpModel`].
///
/// `Io` actions are submitted to the backend, while `Sleep` actions are driven
/// directly by `Lio`'s timer machinery.
pub enum Action {
  Io(Op),
  Sleep(Duration),
}

impl From<Op> for Action {
  fn from(value: Op) -> Self {
    Action::Io(value)
  }
}

impl From<Duration> for Action {
  fn from(value: Duration) -> Self {
    Action::Sleep(value)
  }
}

/// Contract for the execution of one or many [`Action`]s. See [`OpResult`] for
/// complete contract.
///
/// # Safety
///
/// An [`Op`] returned in [`Action::Io`] carries raw pointers that the backend
/// hands to the kernel or to its worker threads, which read and write through
/// them unchecked. For every such `Op` returned from [`action`](Self::action),
/// implementors must guarantee:
///
/// - Every pointer in it, and every pointer in the memory it points to (the
///   `RawBuf`s behind `iovecs`, the `MsgBuf`s behind a `MsgSend`, ...), is
///   valid for the reads and writes its variant describes, over the full
///   length or count the `Op` gives, and is properly aligned. Where the
///   backend reads or replaces a typed value (`SocketAddrBuf`, `FileStat`,
///   `OsString`, `ReadDirResult`, ...), that value is initialized.
/// - That memory stays valid, and is neither moved nor freed, from the moment
///   `action` returns until the operation's completion has been passed to
///   [`complete`](Self::complete), or, if it never is, until the model is
///   dropped. lio keeps the model at one address from before its first
///   `action` call until it is dropped, and does not drop it while an `Op` it
///   returned may still be running, so memory the model owns, inline or on
///   the heap, qualifies. Memory borrowed from outside the model generally
///   does not.
/// - While the operation is in flight, nothing besides the backend accesses
///   memory the operation may write, and nothing writes memory it may read:
///   the model does not share that memory (through an `Arc`, a raw pointer,
///   an item it already handed out, ...) with code that may run meanwhile.
///
/// Models that only return [`Action::Sleep`] or `Op`s without pointers (such
/// as [`Op::Nop`]) uphold this trivially.
///
/// A safe `impl` does not compile, so safe code cannot hand the kernel a
/// wild pointer:
///
/// ```compile_fail,E0200
/// use lio::api::op::{Action, Completion, OpModel, OpResult};
/// use lio::api::resource::Resource;
/// use lio::backend::op::{Op, ReadFlags};
/// use std::ptr::NonNull;
///
/// struct Wild;
///
/// impl OpModel for Wild {
///   type Item = ();
///
///   fn action(&mut self) -> Action {
///     Action::Io(Op::Read {
///       fd: Resource::stdin(),
///       iovecs: NonNull::dangling(),
///       iov_count: 1,
///       offset: -1,
///       flags: ReadFlags::EMPTY,
///     })
///   }
///
///   fn complete(&mut self, _: Completion) -> OpResult<()> {
///     OpResult::Done(())
///   }
/// }
/// ```
pub unsafe trait OpModel: Send + 'static {
  /// The item yielded or completed by this logical operation.
  type Item: Send;

  /// Describe the next runtime action to perform.
  fn action(&mut self) -> Action;

  /// Interpret the completion of the previously submitted low-level step.
  fn complete(&mut self, completion: Completion) -> OpResult<Self::Item>;
}

/// Marker trait for logical operations that produce exactly one final item.
///
/// Types implementing this trait are suitable for the [`Io`](crate::api::Io)
/// wrapper and can be
/// consumed with `.await`.
pub trait OneshotOpModel: OpModel {}

/// Marker trait for logical operations that may yield multiple items over time.
///
/// Types implementing this trait are suitable for the
/// [`IoStream`](crate::api::IoStream) wrapper and
/// are consumed through `.next().await`.
pub trait StreamOpModel: OpModel {}

#[cfg(test)]
mod completion_tests {
  use super::{Completion, CompletionFlags};

  const FLAGS: [CompletionFlags; 5] = [
    CompletionFlags::MORE,
    CompletionFlags::CANCELLED,
    CompletionFlags::TERMINAL,
    CompletionFlags::TIMEOUT,
    CompletionFlags::TIMER,
  ];

  #[test]
  fn flags_are_distinct_bits() {
    let empty = CompletionFlags::empty();
    for (i, a) in FLAGS.into_iter().enumerate() {
      assert_ne!(a, empty);
      assert!(a.contains(a));
      assert!(a.contains(empty));
      assert!(!empty.contains(a));
      assert_eq!(a | a, a);
      for b in FLAGS.into_iter().skip(i + 1) {
        assert_ne!(a, b);
        assert!(!a.contains(b) && !b.contains(a));
        assert!((a | b).contains(a) && (a | b).contains(b));
        assert!(!a.contains(a | b));
      }
    }
  }

  #[test]
  fn completions_compare_their_resource_close() {
    fn other(_: crate::api::resource::RawResource) {}
    let native = Completion::new(3);
    assert_eq!(native, Completion::new(3));
    assert_ne!(native, native.with_resource_close(other));
    assert!(format!("{native:?}").contains("resource_close: ResourceClose"));
  }
}

#[cfg(test)]
mod os_error_tests {
  use super::Completion;

  #[test]
  fn negative_results_map_to_their_os_error() {
    let err = Completion::new(-2).os_error();
    assert_eq!(err.raw_os_error(), Some(2));
  }

  #[test]
  fn out_of_range_results_are_not_wrapped() {
    // Negates to one past `i32::MAX`, where `isize` is wide enough.
    let past_i32 = isize::try_from(-i64::from(i32::MAX) - 2).ok();
    for result in std::iter::once(isize::MIN).chain(past_i32) {
      let err = Completion::new(result).os_error();
      assert_eq!(err.raw_os_error(), None, "{result}");
      assert_eq!(err.kind(), std::io::ErrorKind::Other);
    }
  }
}
