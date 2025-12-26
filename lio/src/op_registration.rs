use std::mem;
// NOTE: OpRegistration should **NEVER** impl Sync.
use std::io;

use std::task::Waker;

use crate::op::{Operation, OperationExt};

pub struct OpCallback {
  callback: *const (),
  call_callback_fn: for<'a> fn(*const (), &'a mut OpRegistration),
}

impl OpCallback {
  pub fn new<T, F>(callback: F) -> Self
  where
    T: OperationExt,
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

unsafe impl Send for OpCallback {}

pub struct OpRegistration {
  status: OpRegistrationStatus,
  stored: StoredOp,
}

unsafe impl Send for OpRegistration {}

#[repr(u8)]
pub enum OpRegistrationStatus {
  Waiting,
  Done { ret: Option<io::Result<i32>> },
}

#[test]
fn test_op_reg_size() {
  assert_eq!(std::mem::size_of::<Notifier>(), 24);
}
// Option's is for ownership rules.
pub enum Notifier {
  Waker(Option<Waker>),
  Callback(OpCallback),
}

impl Notifier {
  pub fn call(self, reg: &mut OpRegistration) {
    match self {
      Self::Waker(Some(waker)) => waker.wake(),
      Self::Waker(None) => {}
      Self::Callback(call) => call.call(reg),
    }
  }
}

pub struct StoredOp {
  op: Box<dyn Operation>,
  notifier: Option<Notifier>,
}

impl StoredOp {
  pub fn op_ref(&self) -> &dyn Operation {
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
    Self { op, notifier: Some(Notifier::Callback(OpCallback::new::<O, F>(f))) }
  }

  pub fn set_waker(&mut self, waker: Waker) {
    match self.notifier.as_mut().expect("what") {
      &mut Notifier::Waker(ref mut old_waker) => {
        *old_waker = Some(waker);
      }
      &mut Notifier::Callback(_) => panic!(),
    }
  }

  pub fn take_notifier(&mut self) -> Notifier {
    self.notifier.take().unwrap()
  }

  pub unsafe fn get_result<R>(&mut self, res: io::Result<i32>) -> R {
    let ptr: *mut () = self.op.result(res).cast_mut();
    let boxed: Box<R> = unsafe { Box::from_raw(ptr.cast()) };
    *boxed
  }
}

impl OpRegistration {
  pub fn new(stored: StoredOp) -> Self {
    Self { stored, status: OpRegistrationStatus::Waiting }
  }
  /// Sets the waker, replacing any existing waker
  pub fn set_waker(&mut self, waker: Waker) {
    match self.status {
      OpRegistrationStatus::Done { ref ret } => {
        assert!(ret.is_some());
        waker.wake();
      }
      OpRegistrationStatus::Waiting => {
        self.stored.set_waker(waker);
      }
    };
  }

  pub fn set_done(&mut self, res: io::Result<i32>) {
    match mem::replace(
      &mut self.status,
      OpRegistrationStatus::Done { ret: Some(res) },
    ) {
      OpRegistrationStatus::Waiting => {
        let noti = self.stored.take_notifier();
        noti.call(self);
      }
      OpRegistrationStatus::Done { .. } => {
        panic!("what");
      }
    }
  }

  // OpRegistrationStatus::DoneWithResultBeforeNotifier { .. } => {
  //   waker.wake();
  //
  //   // try_extract will resolve state and extract result.
  //   return;
  // }
  // OpRegistrationStatus::DoneWithResultAfterNotifier { .. } => {
  //   unreachable!("internal lio: not allowed.")
  // }
  // OpRegistrationStatus::WaitingWithNotifier(Notifier::Callback(_)) => {
  //   unreachable!("lio not allowed: Cannot set callback for waker backed IO")
  // }
  // OpRegistrationStatus::Waiting
  // | OpRegistrationStatus::WaitingWithNotifier(Notifier::Waker(_)) => {
  //   let _ = mem::replace(
  //     &mut self.status,
  //     OpRegistrationStatus::WaitingWithNotifier(Notifier::Waker(Some(
  //       waker,
  //     ))),
  //   );
  // }
  // OpRegistrationStatus::DoneResultTaken => {
  //   panic!("lio: tried setting waker after result taken")
  // }
  // pub fn set_callback(&mut self, callback: OpCallback) {
  //   use std::mem;
  //
  //   match self.status {
  //     OpRegistrationStatus::DoneWithResultBeforeNotifier { .. } => {
  //       callback.call(self);
  //
  //       // try_extract will resolve state and extract result.
  //       return;
  //     }
  //     OpRegistrationStatus::DoneWithResultAfterNotifier { .. } => {
  //       unreachable!("internal lio: not allowed.")
  //     }
  //     OpRegistrationStatus::WaitingWithNotifier(Notifier::Waker(_)) => {
  //       unreachable!("lio not allowed: Cannot set waker for callback backed IO")
  //     }
  //     OpRegistrationStatus::Waiting
  //     | OpRegistrationStatus::WaitingWithNotifier(Notifier::Callback(_)) => {
  //       let _ = mem::replace(
  //         &mut self.status,
  //         OpRegistrationStatus::WaitingWithNotifier(Notifier::Callback(
  //           callback,
  //         )),
  //       );
  //     }
  //     OpRegistrationStatus::DoneResultTaken => {
  //       panic!("lio: tried setting waker after result taken")
  //     }
  //   };
  // }
  // use std::mem;
  //
  // let after_notifier = match self.status {
  //   OpRegistrationStatus::Done { .. } => {
  //     panic!(
  //       "OpRegistration::set_done: Cannot set done on operation already done"
  //     )
  //   }
  //   OpRegistrationStatus::Waiting => self.stored.,
  //   // OpRegistrationStatus::WaitingWithNotifier(_) => true,
  //   // OpRegistrationStatus::DoneResultTaken => {
  //   // }
  // };
  //
  // let which_done = if after_notifier {
  //   OpRegistrationStatus::DoneWithResultAfterNotifier { res }
  // } else {
  //   OpRegistrationStatus::DoneWithResultBeforeNotifier { res }
  // };
  //
  // let old = mem::replace(&mut self.status, which_done);
  //
  // if after_notifier {
  //   if let OpRegistrationStatus::WaitingWithNotifier(value) = old {
  //     return Some(value);
  //   } else {
  //     unreachable!("internal lio error")
  //   }
  // } else {
  //   return None;
  // }
  // pub fn set_done(&mut self, res: io::Result<i32>) -> Option<Notifier> {
  //   use std::mem;
  //
  //   let after_notifier = match self.status {
  //     OpRegistrationStatus::DoneWithResultBeforeNotifier { .. }
  //     | OpRegistrationStatus::DoneWithResultAfterNotifier { .. } => {
  //       panic!(
  //         "OpRegistration::set_done: Cannot set done on operation already done"
  //       )
  //     }
  //     OpRegistrationStatus::Waiting => false,
  //     OpRegistrationStatus::WaitingWithNotifier(_) => true,
  //     OpRegistrationStatus::DoneResultTaken => {
  //       panic!("OpRegistration::set_done: tried to set done after result taken")
  //     }
  //   };
  //
  //   let which_done = if after_notifier {
  //     OpRegistrationStatus::DoneWithResultAfterNotifier { res }
  //   } else {
  //     OpRegistrationStatus::DoneWithResultBeforeNotifier { res }
  //   };
  //
  //   let old = mem::replace(&mut self.status, which_done);
  //
  //   if after_notifier {
  //     if let OpRegistrationStatus::WaitingWithNotifier(value) = old {
  //       return Some(value);
  //     } else {
  //       unreachable!("internal lio error")
  //     }
  //   } else {
  //     return None;
  //   }
  // }

  pub fn run_blocking(&mut self) -> io::Result<i32> {
    self.stored.op_ref().run_blocking()
  }

  pub fn try_extract<T>(&mut self) -> Option<T::Result>
  where
    T: OperationExt,
  {
    match mem::replace(
      &mut self.status,
      OpRegistrationStatus::Done { ret: None },
    ) {
      OpRegistrationStatus::Waiting => None,
      OpRegistrationStatus::Done { ret } => {
        if let Some(res) = ret {
          Some(unsafe { self.stored.get_result::<T::Result>(res) })
        } else {
          panic!();
        }
      }
    }
  }
}
