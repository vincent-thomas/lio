//! What the driver passes between an operation's memory and the backend.

use crate::api::resource::ResourceClose;
use crate::backend::op::BorrowedOp;
#[cfg(test)]
use crate::backend::op::Op;
use std::time::Duration;

/// What the backend reported for one submitted [`Action`].
///
/// The driver creates one from the result an [`IoBackend`] reported, and
/// hands it to the function that reads the result of the operation it
/// submitted for that action. Nothing outside this crate sees one.
///
/// [`IoBackend`]: crate::backend::IoBackend
#[derive(Debug, PartialEq, Eq)]
pub struct Completion {
  result: isize,
  flags: CompletionFlags,
  /// How a resource carried by `result` is closed: the producing backend's
  /// close function.
  resource_close: ResourceClose,
}

impl Completion {
  /// A completion whose resources (if any) are native OS descriptors/handles.
  pub(crate) const fn new(result: isize) -> Self {
    Self::with_flags(result, CompletionFlags::empty())
  }

  pub(crate) const fn with_flags(
    result: isize,
    flags: CompletionFlags,
  ) -> Self {
    Self { result, flags, resource_close: ResourceClose::native() }
  }

  /// The operation's result: non-negative on success, a negated OS error
  /// code on failure.
  pub const fn result(&self) -> isize {
    self.result
  }

  /// The flags the driver set on this completion.
  pub const fn flags(&self) -> CompletionFlags {
    self.flags
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

/// Flags the driver sets on a completion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompletionFlags(u32);

impl CompletionFlags {
  /// The completion is a timer firing, not a backend result.
  pub const TIMER: Self = Self(1 << 4);

  pub const fn empty() -> Self {
    Self(0)
  }

  pub const fn contains(self, other: Self) -> bool {
    (self.0 & other.0) == other.0
  }
}

/// What one operation asks the driver to do.
///
/// `Io` actions are submitted to the backend, while `Sleep` actions are driven
/// directly by `Lio`'s timer machinery.
pub enum Action<'a> {
  Io(BorrowedOp<'a>),
  Sleep(Duration),
}

impl<'a> Action<'a> {
  /// A no-op submission.
  #[cfg(test)]
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

#[cfg(test)]
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

#[cfg(test)]
mod action_tests {
  use super::Action;
  use crate::backend::op::BorrowedOp;
  #[cfg(test)]
  use crate::backend::op::Op;
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

  #[test]
  fn the_timer_flag_is_set_only_when_asked_for() {
    let empty = CompletionFlags::empty();
    let timer = CompletionFlags::TIMER;
    assert_ne!(timer, empty);
    assert!(timer.contains(timer));
    assert!(timer.contains(empty));
    assert!(!empty.contains(timer));
    assert!(!Completion::new(0).flags().contains(timer));
    assert!(Completion::with_flags(0, timer).flags().contains(timer));
  }

  #[test]
  fn completions_compare_their_resource_close() {
    let native = Completion::new(3);
    assert_eq!(native, Completion::new(3));
    assert!(format!("{native:?}").contains("resource_close: ResourceClose"));
    assert_ne!(
      Completion::new(3),
      native.with_resource_close(ResourceClose::noop())
    );
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
