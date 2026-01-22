use std::mem;

mod notifier;
mod stored;

use std::task::Waker;

use crate::operation::{Operation, OperationExt};
pub use stored::StoredOp;

// NOTE: OpRegistration should **NEVER** impl Sync.
pub struct Registration {
  status: Status,
  stored: StoredOp,
}

enum Status {
  Waiting,
  Done { ret: Option<isize> },
}

impl Registration {
  pub(crate) fn op_ref(&self) -> &dyn Operation {
    self.stored.op_ref()
  }
  pub fn new(stored: StoredOp) -> Self {
    Self { stored, status: Status::Waiting }
  }
  /// Sets the waker, replacing any existing waker
  pub fn set_waker(&mut self, waker: Waker) {
    match self.status {
      Status::Done { ref ret } => {
        assert!(ret.is_some());
        waker.wake();
      }
      Status::Waiting => {
        self.stored.set_waker(waker);
      }
    };
  }

  // TODO: Way to remove
  pub fn set_done(&mut self, res: isize) {
    match mem::replace(&mut self.status, Status::Done { ret: Some(res) }) {
      Status::Waiting => {
        let noti = self.stored.take_notifier();
        noti.call(self);
      }
      Status::Done { .. } => {
        panic!("what");
      }
    }
  }

  pub fn run_blocking(&self) -> isize {
    self.stored.op_ref().run_blocking()
  }

  pub fn try_extract<T>(&mut self) -> Option<T::Result>
  where
    T: OperationExt,
  {
    match mem::replace(&mut self.status, Status::Done { ret: None }) {
      Status::Waiting => None,
      Status::Done { ret } => {
        if let Some(res) = ret {
          // SAFETY: The stored operation's type matches T because this is only called
          // from Io<T>, which ensures type consistency. The result value `res` is
          // valid because it was set via set_done() and we're extracting it exactly once.
          Some(unsafe { self.stored.get_result::<T::Result>(res) })
        } else {
          panic!();
        }
      }
    }
  }
}
