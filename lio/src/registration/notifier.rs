use std::task::Waker;

use crate::typed_op::TypedOp;

use super::Registration;

// Option's is for ownership rules.
pub(crate) enum Notifier {
  Waker(Option<Waker>),
  Callback(OpCallback),
}

impl Notifier {
  pub fn new_waker(waker: Waker) -> Self {
    Self::Waker(Some(waker))
  }
  pub fn new_callback<T, F>(callback: F, typed_op: T) -> Self
  where
    T: TypedOp,
    F: FnOnce(T::Result) + Send,
  {
    Self::Callback(OpCallback::new::<T, F>(callback, typed_op))
  }
  pub fn set_waker(&mut self, waker: Waker) -> bool {
    match self {
      Self::Waker(old) => {
        let _ = old.replace(waker);
        true
      }
      Self::Callback(_) => false,
    }
  }
  pub fn call(self, reg: &mut Registration) {
    match self {
      Self::Waker(Some(waker)) => waker.wake(),
      Self::Waker(None) => {}
      Self::Callback(call) => call.call(reg),
    }
  }
}

pub(crate) struct OpCallback {
  callback: *const (),
  typed_op: *const (),
  call_callback_fn:
    for<'a> fn(*const (), *const (), isize, &'a mut Registration),
}

impl Drop for OpCallback {
  fn drop(&mut self) {
    // If the callback wasn't called, we need to clean up the stored pointers
    if !self.callback.is_null() {
      // SAFETY: We need to drop the boxed callback to prevent memory leaks
      // This is only called when OpCallback is dropped without `call` being invoked
      // unsafe { drop(Box::from_raw(self.callback as *mut ())); }
    }
    if !self.typed_op.is_null() {
      // SAFETY: We need to drop the boxed typed_op to prevent memory leaks
      // unsafe { drop(Box::from_raw(self.typed_op as *mut ())); }
    }
  }
}

// SAFETY: OpCallback is Send because:
// - The callback pointer points to a `F: FnOnce(T::Result) + Send` type (boxed in `new`)
// - The typed_op pointer points to a `T: TypedOp + Send` type (boxed in `new`)
// - We maintain exclusive ownership and only call them once via `call`
// - The function pointer is a static function, which is Send
unsafe impl Send for OpCallback {}
// SAFETY: OpCallback is Sync because:
// - The callback and typed_op are only called once and consumed via `call` (takes self, not &self)
// - The function pointer is a static function pointer, which is Sync
// - While the callback itself may not be Sync, we never access it through a shared reference
unsafe impl Sync for OpCallback {}

impl OpCallback {
  fn new<T, F>(callback: F, typed_op: T) -> Self
  where
    T: TypedOp,
    F: FnOnce(T::Result) + Send,
  {
    OpCallback {
      callback: Box::into_raw(Box::new(callback)) as *const (),
      typed_op: Box::into_raw(Box::new(typed_op)) as *const (),
      call_callback_fn: Self::call_callback::<T, F>,
    }
  }

  pub fn call(self, reg: &mut Registration) {
    let res = reg
      .try_take_result()
      .expect("Result should be available when callback is called");
    (self.call_callback_fn)(self.callback, self.typed_op, res, reg);
  }

  fn call_callback<T, F>(
    callback_ptr: *const (),
    typed_op_ptr: *const (),
    res: isize,
    _reg: &mut Registration,
  ) where
    T: TypedOp,
    F: FnOnce(T::Result),
  {
    // SAFETY: We created this pointer with Box::into_raw from a Box<TypedOp>
    let typed_op = unsafe { Box::from_raw(typed_op_ptr as *mut T) };
    let result = typed_op.extract_result(res);
    // SAFETY: We created this pointer with Box::into_raw from a Box<dyn FnOnce(T::Result) + Send>
    let callback = unsafe { Box::from_raw(callback_ptr as *mut F) };
    callback(result)
  }
}

#[test]
fn test_op_reg_size() {
  assert_eq!(std::mem::size_of::<Notifier>(), 24);
}
