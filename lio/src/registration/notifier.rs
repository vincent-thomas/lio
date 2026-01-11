use std::task::Waker;

use crate::operation::OperationExt;

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
  pub fn new_callback<T, F>(callback: F) -> Self
  where
    T: OperationExt,
    F: FnOnce(T::Result) + Send,
  {
    Self::Callback(OpCallback::new::<T, F>(callback))
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
  call_callback_fn: for<'a> fn(*const (), &'a mut Registration),
}

unsafe impl Send for OpCallback {}
unsafe impl Sync for OpCallback {}

impl OpCallback {
  fn new<T, F>(callback: F) -> Self
  where
    T: OperationExt,
    F: FnOnce(T::Result) + Send,
  {
    OpCallback {
      callback: Box::into_raw(Box::new(callback)) as *const (),
      call_callback_fn: Self::call_callback::<T, F>,
    }
  }

  pub fn call(self, reg: &mut Registration) {
    (self.call_callback_fn)(self.callback, reg);
  }

  fn call_callback<T, F>(callback_ptr: *const (), reg: &mut Registration)
  where
    T: OperationExt,
    F: FnOnce(T::Result),
  {
    match reg.try_extract::<T>() {
      Some(res) => {
        // SAFETY: We created this pointer with Box::into_raw from a Box<dyn FnOnce(T::Result) + Send>
        let callback = unsafe { Box::from_raw(callback_ptr as *mut F) };
        callback(res)
      }
      None => panic!(
        "internal lio error: it is illegal for TryExtractOutcome to be StillWaiting when callback is called"
      ),
    };
  }
}

#[test]
fn test_op_reg_size() {
  assert_eq!(std::mem::size_of::<Notifier>(), 24);
}
