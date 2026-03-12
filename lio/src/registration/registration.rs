use std::mem;

pub mod notifier;
// mod stored;

use std::task::Waker;

use crate::{
  registration::notifier::{Notifier, OpCallback},
  typed_op::TypedOp,
};

/// Opaque wrapper that hides the `pub(crate)` `Notifier` from the public `Registration` enum.
pub struct RegistrationInner {
  pub(crate) notifier: Notifier,
}

// NOTE: OpRegistration should **NEVER** impl Sync.
pub enum Registration {
  Pending(RegistrationInner),
  Done(Option<isize>),
}

impl Registration {
  pub fn new_waker(waker: Waker) -> Self {
    Self::Pending(RegistrationInner { notifier: Notifier::Waker(Some(waker)) })
  }

  pub fn new_callback<T, F>(callback: F, typed_op: T) -> Self
  where
    T: TypedOp,
    F: FnOnce(T::Result) + Send,
  {
    Self::Pending(RegistrationInner {
      notifier: Notifier::Callback(OpCallback::new::<T, F>(callback, typed_op)),
    })
  }

  /// Create a callback registration from an already-boxed TypedOp.
  ///
  /// This is the preferred method when the TypedOp has already been boxed
  /// to establish a stable address before calling `into_op()`. This ensures
  /// pointers in the Op remain valid since the TypedOp won't move after boxing.
  pub fn new_callback_boxed<T, F>(callback: F, typed_op: Box<T>) -> Self
  where
    T: TypedOp,
    F: FnOnce(T::Result) + Send,
  {
    Self::Pending(RegistrationInner {
      notifier: Notifier::Callback(OpCallback::new_boxed::<T, F>(
        callback, typed_op,
      )),
    })
  }

  /// Sets the waker, replacing any existing waker
  pub fn set_waker(&mut self, waker: Waker) {
    match self {
      Self::Done(ret) => {
        assert!(ret.is_some());
        waker.wake();
      }
      Self::Pending(RegistrationInner { notifier, .. }) => {
        notifier.set_waker(waker);
      }
    };
  }

  // TODO: Way to remove
  pub fn set_done(&mut self, res: isize) {
    match mem::replace(self, Self::Done(Some(res))) {
      Self::Pending(RegistrationInner { notifier }) => {
        notifier.call(self);
      }
      Self::Done { .. } => {
        panic!("what");
      }
    }
  }

  pub fn try_take_result(&mut self) -> Option<isize> {
    match self {
      Self::Done(t) => Some(t.take().expect("Already taken")),
      Self::Pending(_) => None,
    }
  }

  /// Returns true if this registration has completed and its result was consumed.
  /// This happens for callbacks which consume the result immediately in set_done.
  pub fn result_consumed(&self) -> bool {
    matches!(self, Self::Done(None))
  }
}
