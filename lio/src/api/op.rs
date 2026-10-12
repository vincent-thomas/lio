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
pub trait OpModel: Send + 'static {
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
