// NOTE: OpRegistration should **NEVER** impl Sync.
use std::{io, mem, ptr::NonNull};

#[cfg(feature = "high")]
use std::task::Waker;

use crate::op::Operation;

pub struct OpCallback {
  callback: *const (),
  call_callback_fn: fn(*const (), &mut OpRegistration),
}

impl OpCallback {
  pub fn new<T, F>(callback: F) -> Self
  where
    T: Operation,
    F: FnOnce(T::Result) + Send,
  {
    OpCallback {
      callback: Box::into_raw(Box::new(callback)) as *const (),
      call_callback_fn: Self::call_callback::<T, F>,
    }
  }

  pub fn call(self, reg: &mut OpRegistration) {
    (self.call_callback_fn)(self.callback, reg);
  }

  fn call_callback<T, F>(callback_ptr: *const (), reg: &mut OpRegistration)
  where
    T: Operation,
    F: FnOnce(T::Result),
  {
    // SAFETY: We created this pointer with Box::into_raw from a Box<dyn FnOnce(T::Result) + Send>
    let callback = unsafe { Box::from_raw(callback_ptr as *mut F) };

    match reg.try_extract::<T>() {
      Some(res) => callback(res),
      None => panic!(
        "internal lio error: it is illegal for TryExtractOutcome to be StillWaiting when callback is called"
      ),
    };
  }
}

unsafe impl Send for OpCallback {}

pub struct OpRegistration {
  status: OpRegistrationStatus,
  op: Option<NonNull<()>>,
  op_fn_drop: fn(*const ()), // Function to properly drop the operation
  op_fn_run_blocking: fn(*const ()) -> std::io::Result<i32>,
}

impl Drop for OpRegistration {
  fn drop(&mut self) {
    if let Some(operation) = self.op.take() {
      (self.op_fn_drop)(operation.as_ptr());
    }
  }
}

unsafe impl Send for OpRegistration {}

pub enum OpRegistrationStatus {
  Waiting,
  WaitingWithNotifier(OpNotification),
  DoneWithResultBeforeNotifier { res: io::Result<i32> },
  DoneWithResultAfterNotifier { res: io::Result<i32> },
  DoneResultTaken,
}

#[test]
fn test_op_reg_size() {
  assert_eq!(std::mem::size_of::<OpNotification>(), 24);
}
// Option's is for ownership rules.
pub enum OpNotification {
  #[cfg(feature = "high")]
  Waker(Waker),
  Callback(OpCallback),
}

impl OpRegistration {
  fn op_ptr(&self) -> *const () {
    self.op.expect("trying to run run_blocking after result").as_ptr()
  }

  pub fn new<T>(op: Box<T>) -> Self
  where
    T: Operation,
  {
    fn drop_op<T>(ptr: *const ()) {
      drop(unsafe { Box::from_raw(ptr as *mut T) })
    }

    fn op_fn_run_blocking<T>(ptr: *const ()) -> std::io::Result<i32>
    where
      T: Operation,
    {
      let op: &T = unsafe { &*(ptr as *const T) };
      op.run_blocking()
    }

    OpRegistration {
      op: Some(NonNull::new(Box::into_raw(op) as *mut ()).unwrap()),
      op_fn_drop: drop_op::<T>,
      op_fn_run_blocking: op_fn_run_blocking::<T>,
      status: OpRegistrationStatus::Waiting,
    }
  }

  pub fn run_blocking(&mut self) -> io::Result<i32> {
    (self.op_fn_run_blocking)(self.op_ptr())
  }

  /// Returns Some(...) if done, None if not
  pub fn try_extract<T>(&mut self) -> Option<T::Result>
  where
    T: Operation,
  {
    match mem::replace(&mut self.status, OpRegistrationStatus::DoneResultTaken)
    {
      OpRegistrationStatus::Waiting
      | OpRegistrationStatus::WaitingWithNotifier(_) => None,
      OpRegistrationStatus::DoneResultTaken => panic!(
        "lio internal error: tried to extract error when result has been taken."
      ),
      OpRegistrationStatus::DoneWithResultBeforeNotifier { res }
      | OpRegistrationStatus::DoneWithResultAfterNotifier { res } => {
        let ptr = self.op.take().expect("guarranteed not to panic, because we have owned and drop can't be called.").as_ptr();
        let mut op = unsafe { Box::from_raw(ptr as *mut T) };
        Some(op.result(res))
      }
    }
  }

  /// Sets the waker, replacing any existing waker
  #[cfg(feature = "high")]
  pub fn set_waker(&mut self, waker: Waker) {
    use std::mem;

    match self.status {
      OpRegistrationStatus::DoneWithResultBeforeNotifier { .. } => {
        waker.wake();

        // try_extract will resolve state and extract result.
        return;
      }
      OpRegistrationStatus::DoneWithResultAfterNotifier { .. } => {
        unreachable!("internal lio: not allowed.")
      }
      OpRegistrationStatus::WaitingWithNotifier(OpNotification::Callback(
        _,
      )) => {
        unreachable!("lio not allowed: Cannot set callback for waker backed IO")
      }
      OpRegistrationStatus::Waiting
      | OpRegistrationStatus::WaitingWithNotifier(OpNotification::Waker(_)) => {
        let _ = mem::replace(
          &mut self.status,
          OpRegistrationStatus::WaitingWithNotifier(OpNotification::Waker(
            waker,
          )),
        );
      }
      OpRegistrationStatus::DoneResultTaken => {
        panic!("lio: tried setting waker after result taken")
      }
    };
  }
  pub fn set_callback(&mut self, callback: OpCallback) {
    use std::mem;

    match self.status {
      OpRegistrationStatus::DoneWithResultBeforeNotifier { .. } => {
        callback.call(self);

        // try_extract will resolve state and extract result.
        return;
      }
      OpRegistrationStatus::DoneWithResultAfterNotifier { .. } => {
        unreachable!("internal lio: not allowed.")
      }
      OpRegistrationStatus::WaitingWithNotifier(OpNotification::Waker(_)) => {
        unreachable!("lio not allowed: Cannot set waker for callback backed IO")
      }
      OpRegistrationStatus::Waiting
      | OpRegistrationStatus::WaitingWithNotifier(OpNotification::Callback(
        _,
      )) => {
        let _ = mem::replace(
          &mut self.status,
          OpRegistrationStatus::WaitingWithNotifier(OpNotification::Callback(
            callback,
          )),
        );
      }
      OpRegistrationStatus::DoneResultTaken => {
        panic!("lio: tried setting waker after result taken")
      }
    };
  }

  pub fn set_done(&mut self, res: io::Result<i32>) -> Option<OpNotification> {
    use std::mem;

    let after_notifier = match self.status {
      OpRegistrationStatus::DoneWithResultBeforeNotifier { .. }
      | OpRegistrationStatus::DoneWithResultAfterNotifier { .. } => {
        unreachable!("lio not allowed: Cannot set done on thing already done.")
      }
      OpRegistrationStatus::Waiting => false,
      OpRegistrationStatus::WaitingWithNotifier(_) => true,
      OpRegistrationStatus::DoneResultTaken => {
        panic!("lio: tried to set done after result taken")
      }
    };

    let which_done = if after_notifier {
      OpRegistrationStatus::DoneWithResultAfterNotifier { res }
    } else {
      OpRegistrationStatus::DoneWithResultBeforeNotifier { res }
    };

    let old = mem::replace(&mut self.status, which_done);

    if after_notifier {
      if let OpRegistrationStatus::WaitingWithNotifier(value) = old {
        return Some(value);
      } else {
        unreachable!("internal lio error")
      }
    } else {
      return None;
    }
  }
}
