//! Type-safe operation traits for bridging user types to backend execution.

use crate::backend::op::Op;
use std::time::Duration;

/// Completion metadata routed from the backend to one `OpModel`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Completion {
  pub result: isize,
  pub flags: CompletionFlags,
}

impl Completion {
  pub const fn new(result: isize) -> Self {
    Self { result, flags: CompletionFlags::empty() }
  }

  pub const fn with_flags(result: isize, flags: CompletionFlags) -> Self {
    Self { result, flags }
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
