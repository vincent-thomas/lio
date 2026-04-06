//! Type-safe operation traits for bridging user types to backend execution.

use crate::backend::op::Op;
use std::time::Duration;

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

// impl OpFlow {
//   pub fn send_once(prev_result: Option<isize>, op: Op) -> Self {
//     if prev_result.is_none() { Self::Send(op) } else { Self::Done }
//   }
// }

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

/// Result of interpreting one completion.
pub enum OpResult<T> {
  /// Ask the runtime for the next action by calling `OpModel::action()` again.
  Again,
  /// Yield one item but keep the model alive.
  Yield(T),
  /// Finish the model with its final output.
  Done(T),
}

/// One runtime action requested by an [`OpModel`].
///
/// `Io` actions are submitted to the backend, while `Sleep` actions are driven
/// directly by `Lio`'s timer machinery.
pub enum Action {
  Io(Op),
  Sleep(Duration),
}

/// One scripted contract step for a serial `OpModel`.
pub struct ContractStep<M: OpModel> {
  pub assert_action: fn(&Action) -> bool,
  pub before_complete: fn(&mut M),
  pub completion: Completion,
  pub assert_result: fn(&OpResult<M::Item>) -> bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContractKind {
  Oneshot,
  Stream,
}

impl<M: OpModel> ContractStep<M> {
  pub fn new(
    assert_action: fn(&Action) -> bool,
    completion: Completion,
    assert_result: fn(&OpResult<M::Item>) -> bool,
  ) -> Self {
    Self {
      assert_action,
      before_complete: |_| {},
      completion,
      assert_result,
    }
  }

  pub fn with_setup(
    assert_action: fn(&Action) -> bool,
    before_complete: fn(&mut M),
    completion: Completion,
    assert_result: fn(&OpResult<M::Item>) -> bool,
  ) -> Self {
    Self { assert_action, before_complete, completion, assert_result }
  }
}

/// Test fixture for generating generic `OpModel` contract tests per type.
pub trait OpModelContract: OpModel + Sized {
  fn contract_kind() -> ContractKind;
  fn contract_model() -> Self;
  fn contract_steps() -> Vec<ContractStep<Self>>;
}

#[macro_export]
macro_rules! test_op_model_contract {
  ($model_ty:ty) => {
    mod op_model_contract {
      use super::*;

      #[test]
      fn scripted_contract() {
        let mut model =
          <$model_ty as $crate::api::op::OpModelContract>::contract_model();
        let kind =
          <$model_ty as $crate::api::op::OpModelContract>::contract_kind();
        let steps =
          <$model_ty as $crate::api::op::OpModelContract>::contract_steps();
        assert!(!steps.is_empty(), "contract script must not be empty");

        let mut saw_done = false;
        let mut saw_yield = false;
        let mut saw_terminal = false;
        let mut must_remain_live_after_script = false;
        let last_index = steps.len() - 1;

        for (index, step) in steps.into_iter().enumerate() {
          assert!(
            !saw_terminal,
            "contract script continued after terminal completion"
          );
          let action = model.action();
          assert!(
            (step.assert_action)(&action),
            "action() did not satisfy the model contract"
          );
          (step.before_complete)(&mut model);
          let result = model.complete(step.completion);
          assert!(
            (step.assert_result)(&result),
            "complete() did not satisfy the model contract"
          );

          match result {
            $crate::api::op::OpResult::Again => {
              must_remain_live_after_script = index == last_index;
            }
            $crate::api::op::OpResult::Yield(_) => {
              saw_yield = true;
              must_remain_live_after_script = index == last_index;
              assert!(
                kind != $crate::api::op::ContractKind::Oneshot,
                "oneshot models must not yield"
              );
            }
            $crate::api::op::OpResult::Done(_) => {
              saw_done = true;
              saw_terminal = true;
              assert_eq!(
                index,
                last_index,
                "Done must be the final scripted step"
              );
            }
          }
        }

        if must_remain_live_after_script {
          let _ = model.action();
        }

        match kind {
          $crate::api::op::ContractKind::Oneshot => {
            assert!(
              saw_done,
              "oneshot model contract must terminate with Done"
            );
            assert!(
              !saw_yield,
              "oneshot model contract must not contain Yield"
            );
          }
          $crate::api::op::ContractKind::Stream => {
            assert!(
              saw_yield || saw_done,
              "stream model contract must produce at least one Yield or Done"
            );
          }
        }
      }
    }
  };
}

/// Contract for the execution of one or many  [`Op`]s.
///
/// Because of this, this means that this trait can define a "oneshot-like"
/// operation, but can also model streaming, for example AcceptMulti.
///
pub trait OpModel: Send + Sync + 'static {
  /// The item yielded or completed by this logical operation.
  type Item: Send + Sync;

  /// Describe the next runtime action to perform.
  fn action(&mut self) -> Action;

  /// Interpret the completion of the previously submitted low-level step.
  fn complete(&mut self, completion: Completion) -> OpResult<Self::Item>;
}

/// Marker trait for logical operations that produce exactly one final item.
///
/// Types implementing this trait are suitable for the [`Io`] wrapper and can be
/// consumed with `.await`.
pub trait OneshotOpModel: OpModel {}

/// Marker trait for logical operations that may yield multiple items over time.
///
/// Types implementing this trait are suitable for the [`IoStream`] wrapper and
/// are consumed through `.next().await`.
pub trait StreamOpModel: OpModel {}
