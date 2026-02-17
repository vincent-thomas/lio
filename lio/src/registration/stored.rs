use std::task::Waker;

use crate::{op::Op, registration::notifier::Notifier, typed_op::TypedOp};

/// Type-safe stored operation with compile-time result type guarantees.
///
/// This stores an operation along with its complete type information, allowing
/// for zero-cost, type-safe result extraction without any unsafe code.
pub struct StoredOp {
  op: Option<Op>,
  /// Cached result after completion, before extraction
  result: Option<OpResult>,
  notifier: Option<Notifier>,
}

impl std::fmt::Display for ResultNotMatching {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    write!(
      f,
      "Result type mismatch: expected to extract {} but got {}",
      self.expected, self.actual
    )
  }
}

impl std::error::Error for ResultNotMatching {
  fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
    None
  }

  fn description(&self) -> &str {
    &self.to_string()
  }
}

impl StoredOp {
  /// Create a new StoredOp for async operations with waker.
  ///
  /// # Type Safety
  ///
  /// The type marker is determined from the operation type T, ensuring
  /// that result extraction can only return the correct type.
  pub fn new_waker<T: TypedOp>(op: Op, waker: Waker) -> Self {
    Self {
      op: Some(op),
      result: None,
      notifier: Some(Notifier::new_waker(waker)),
    }
  }

  /// Create a new StoredOp for callback-based operations.
  ///
  /// # Type Safety
  ///
  /// The type marker ensures that the callback receives the correct result type.
  pub fn new_callback<T: TypedOp, F: FnOnce(T::Result) + Send>(
    op: Op,
    f: F,
  ) -> Self {
    Self {
      op: Some(op),
      result: None,
      notifier: Some(Notifier::new_callback::<T, F>(f)),
    }
  }

  pub fn op_ref(&self) -> &Op {
    self.op.as_ref().expect("Operation already consumed")
  }

  pub fn set_waker(&mut self, waker: Waker) -> bool {
    match self {
      Self { ref mut notifier, .. } => notifier.set_waker(waker),
      Self::Result(_) => panic!(),
    }
  }

  pub(crate) fn take_notifier(&mut self) -> Notifier {
    self.notifier.take().unwrap()
  }

  /// Type-safe result extraction using TypedOp.
  ///
  /// This method provides complete compile-time type safety by ensuring
  /// that the stored operation type matches the expected extraction type.
  pub fn get_op_result(&mut self, res: isize) -> OpResult {
    let op = self.op.take().expect("Operation result already extracted");
    op.result(res)
  }

  /// Type-safe result extraction for a specific operation type.
  ///
  /// # Type Safety
  ///
  /// This method validates that the stored operation type matches the
  /// expected result type T, then performs type-safe extraction.
  /// The compiler prevents calling this with wrong operation type.
  pub fn get_typed_result<T: TypedOp>(
    &mut self,
    res: isize,
  ) -> Option<T::Result> {
    // Validate that stored operation type matches expected type
    if Self::type_marker_for::<T>() != self.type_marker {
      return None;
    }

    // Compute OpResult if not already cached
    if self.result.is_none() {
      let op_result = self.get_op_result(res);
      self.result = Some(op_result);
    }

    // Extract using operation's type-safe method
    let op_result = self.result.as_mut().unwrap();
    op_result.into_typed()
  }
}

impl StoredOp {
  pub fn op_ref(&self) -> &Op {
    match self {
      Self::Pending { ref op, .. } => op,
      Self::Result(_) => panic!(),
    }
  }

  /// Create a new StoredOp for async operations with waker.
  ///
  /// # Type Safety
  ///
  /// The type marker is determined from the operation type T, ensuring
  /// that result extraction can only return the correct type.
  pub fn new_waker<T: TypedOp>(op: Op, waker: Waker) -> Self {
    Self::Pending { op, notifier: Notifier::new_waker(waker) }
  }

  /// Create a new StoredOp for callback-based operations.
  ///
  /// # Type Safety
  ///
  /// The type marker ensures that the callback receives the correct result type.
  pub fn new_callback<T: TypedOp, F: FnOnce(T::Result) + Send>(
    op: Op,
    f: F,
  ) -> Self {
    Self::Pending { op, notifier: Notifier::new_callback::<T, F>(f) }
  }

  pub fn set_waker(&mut self, waker: Waker) -> bool {
    match self {
      Self::Pending { ref mut notifier, .. } => notifier.set_waker(waker),
      Self::Result(_) => panic!(),
    }
  }

  pub fn result(&mut self) -> Option<isize> {
    match self {
      Self::Pending { .. } => None,
      Self::Result(result) => Some(*result),
    }
  }
}
