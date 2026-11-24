//! Type-safe operation traits for bridging user types to backend execution.

use crate::api::resource::ResourceClose;
use crate::backend::op::{BorrowedOp, Op};
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
    Self { result, flags, resource_close: ResourceClose::native() }
  }

  /// Sets how a resource carried by this completion is closed.
  pub(crate) const fn with_resource_close(
    mut self,
    close: ResourceClose,
  ) -> Self {
    self.resource_close = close;
    self
  }

  /// How a resource carried by this completion is closed.
  pub(crate) const fn resource_close(&self) -> ResourceClose {
    self.resource_close
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
pub enum Action<'a> {
  Io(BorrowedOp<'a>),
  Sleep(Duration),
}

impl<'a> Action<'a> {
  /// A no-op submission.
  pub fn nop() -> Action<'static> {
    Action::Io(BorrowedOp::nop())
  }

  /// Drops the borrow brand.
  ///
  /// # Safety
  ///
  /// The memory `self` borrows must remain valid for as long as the returned
  /// action is used. Contract tests call this to inspect an action while the
  /// model is still borrowed, and they never submit the erased action.
  #[cfg(test)]
  pub(crate) unsafe fn erase_lifetime(self) -> Action<'static> {
    // SAFETY: `Action` stores raw pointers inside `Op` plus a `PhantomData`
    // brand. Extending the brand does not create a reference.
    unsafe { std::mem::transmute(self) }
  }
}

impl<'a> Action<'a> {
  /// The submitted operation, when this action is I/O.
  pub fn as_op(&self) -> Option<&Op> {
    match self {
      Action::Io(op) => Some(op.as_op()),
      Action::Sleep(_) => None,
    }
  }

  /// Whether this action submits [`Op::Nop`](crate::backend::op::Op::Nop).
  pub fn is_nop(&self) -> bool {
    matches!(self.as_op(), Some(Op::Nop))
  }
}

impl<'a> From<BorrowedOp<'a>> for Action<'a> {
  fn from(value: BorrowedOp<'a>) -> Self {
    Action::Io(value)
  }
}

impl<'a> From<Duration> for Action<'a> {
  fn from(value: Duration) -> Self {
    Action::Sleep(value)
  }
}

/// Contract for the execution of one or many [`Action`]s. See [`OpResult`] for
/// complete contract.
pub trait OpModel: Send + 'static {
  /// The item yielded or completed by this logical operation.
  type Item: Send;

  /// Describe the next runtime action to perform.
  ///
  /// Pointers in the returned action borrow `self`. The driver keeps `self`
  /// pinned, and does not call `action` again, until the backend has finished
  /// that submission.
  fn action(&mut self) -> Action<'_>;

  /// Interpret the completion of the previously submitted low-level step.
  ///
  /// # Safety
  ///
  /// `completion` is the backend result for the [`Action`] returned by the
  /// matching [`action`](Self::action) call, and the buffers that action
  /// named are still in place. For a read, `result` is the number of bytes
  /// the backend wrote into those buffers. For an operation that returns a
  /// resource, `result` is a descriptor or handle this completion owns.
  unsafe fn complete(&mut self, completion: Completion)
  -> OpResult<Self::Item>;
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
mod action_tests {
  use super::Action;
  use crate::backend::op::{BorrowedOp, Op};
  use std::time::Duration;

  #[test]
  fn nop_is_a_nop() {
    let action = Action::nop();
    assert!(action.is_nop());
    assert!(matches!(action.as_op(), Some(Op::Nop)));
  }

  #[test]
  fn sleep_is_not_io() {
    let action = Action::from(Duration::from_millis(3));
    assert!(action.as_op().is_none());
    assert!(!action.is_nop());
    assert!(
      matches!(action, Action::Sleep(duration) if duration == Duration::from_millis(3))
    );
  }

  #[test]
  fn borrowed_nop_round_trips() {
    let action = Action::from(BorrowedOp::nop());
    assert!(action.is_nop());
  }
}

#[cfg(test)]
mod completion_tests {
  use super::{Completion, CompletionFlags};
  use crate::api::resource::ResourceClose;

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
    let native = Completion::new(3);
    assert_eq!(native, Completion::new(3));
    assert_ne!(native, native.with_resource_close(ResourceClose::noop()));
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
