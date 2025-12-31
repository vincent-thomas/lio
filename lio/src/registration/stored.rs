use std::task::Waker;

use crate::{
  op::{Operation, OperationExt},
  registration::notifier::Notifier,
};

pub struct StoredOp {
  op: Box<dyn Operation>,
  notifier: Option<Notifier>,
}

impl StoredOp {
  pub fn op_ref<'a>(&'a self) -> &'a dyn Operation {
    self.op.as_ref()
  }
  pub fn new_waker<O>(op: O) -> Self
  where
    O: OperationExt + 'static,
  {
    Self { op: Box::new(op), notifier: Some(Notifier::Waker(None)) }
  }
  pub fn new_callback<O, F>(op: Box<O>, f: F) -> Self
  where
    O: OperationExt + 'static,
    F: FnOnce(O::Result) + Send,
  {
    Self { op, notifier: Some(Notifier::new_callback::<O, F>(f)) }
  }

  pub fn set_waker(&mut self, waker: Waker) -> bool {
    let Some(notifier) = self.notifier.as_mut() else {
      return false;
    };

    assert!(
      notifier.set_waker(waker),
      "lio inner logic error: Tried setting waker for callback backed op"
    );

    true
  }

  pub fn take_notifier(&mut self) -> Notifier {
    self.notifier.take().unwrap()
  }

  /// Caller guarrantees that type 'R' is the same as Box<dyn Operation>::result -> *const R is.
  pub unsafe fn get_result<R>(&mut self, res: isize) -> R {
    let ptr: *mut () = self.op.result(res).cast_mut();
    // SAFETY: Callers responsibility.
    let boxed: Box<R> = unsafe { Box::from_raw(ptr.cast()) };
    *boxed
  }
}
