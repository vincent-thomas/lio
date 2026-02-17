use std::mem;

pub mod notifier;
// mod stored;

use std::task::Waker;

use crate::registration::notifier::Notifier;

// NOTE: OpRegistration should **NEVER** impl Sync.
pub enum Registration {
  Pending { notifier: Notifier },
  Done(Option<isize>),
}

impl Registration {
  pub fn new(notifier: Notifier) -> Self {
    Self::Pending { notifier }
  }

  /// Sets the waker, replacing any existing waker
  pub fn set_waker(&mut self, waker: Waker) {
    match self {
      Self::Done(ret) => {
        assert!(ret.is_some());
        waker.wake();
      }
      Self::Pending { notifier, .. } => {
        notifier.set_waker(waker);
      }
    };
  }

  // TODO: Way to remove
  pub fn set_done(&mut self, res: isize) {
    match mem::replace(self, Self::Done(Some(res))) {
      Self::Pending { notifier } => {
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
      Self::Pending { .. } => None,
    }
  }
}
