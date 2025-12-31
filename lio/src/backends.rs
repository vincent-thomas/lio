use std::{io, sync::Arc};

use crate::{op::Operation, registration::StoredOp, store::OpStore};

#[cfg(linux)]
mod io_uring;
#[cfg(linux)]
pub use io_uring::*;

#[cfg(windows)]
mod iocp;
#[cfg(windows)]
pub use iocp::*;

pub mod blocking;
#[cfg(test)]
pub mod dummy;
pub mod pollingv2;
// mod polling;
// pub use polling::*;

#[derive(Debug)]
pub enum SubmitErr {
  Io(io::Error),
  Full,
  NotCompatible,
  DriverShutdown,
}

pub enum SubmitErrExt {
  NotCompatible(StoredOp),
  SubmitErr(SubmitErr),
}

impl std::fmt::Debug for SubmitErrExt {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::NotCompatible(_) => {
        f.debug_tuple("NotCompatible").field(&"<StoredOp>").finish()
      }
      Self::SubmitErr(err) => f.debug_tuple("SubmitErr").field(err).finish(),
    }
  }
}

impl From<io::Error> for SubmitErr {
  fn from(value: io::Error) -> Self {
    Self::Io(value)
  }
}

pub struct OpCompleted {
  pub op_id: u64,
  /// Result of the operation:
  /// - >= 0 on success (the result value)
  /// - < 0 on error (negative errno value)
  pub result: isize,
}

impl OpCompleted {
  pub fn new(op_id: u64, result: isize) -> Self {
    Self { op_id, result }
  }
}

pub trait IoDriver {
  type Submitter: IoSubmitter + 'static;
  type Handler: IoHandler + 'static;

  fn new_state() -> io::Result<*const ()>;
  fn drop_state(state: *const ());

  fn new(state: *const ()) -> io::Result<(Self::Submitter, Self::Handler)>;
}

pub trait IoSubmitter: Send {
  fn submit(
    &mut self,
    state: *const (),
    id: u64,
    op: &dyn Operation,
  ) -> Result<(), SubmitErr>;

  /// TODO: move to already existing id api.
  fn notify(&mut self, state: *const ()) -> Result<(), SubmitErr>;
}

pub trait IoHandler: Send {
  fn try_tick(
    &mut self,
    state: *const (),
    store: &OpStore,
  ) -> io::Result<Vec<OpCompleted>>;
  fn tick(
    &mut self,
    state: *const (),
    store: &OpStore,
  ) -> io::Result<Vec<OpCompleted>>;
}

pub struct Submitter {
  sub: Box<dyn IoSubmitter>,
  store: Arc<OpStore>,
  state: *const (),
}

unsafe impl Send for Submitter {}

impl Submitter {
  pub fn new(
    sub: Box<dyn IoSubmitter>,
    store: Arc<OpStore>,
    state: *const (),
  ) -> Self {
    Self { sub, store, state }
  }

  pub fn submit(&mut self, op: StoredOp) -> Result<u64, SubmitErrExt> {
    self.store.insert_with_try(|id| {
      match self.sub.submit(self.state, id, op.op_ref()) {
        Ok(()) => Ok(op),
        Err(err) => match err {
          SubmitErr::NotCompatible => Err(SubmitErrExt::NotCompatible(op)),
          _ => Err(SubmitErrExt::SubmitErr(err)),
        },
      }
    })
  }

  pub fn notify(&mut self) -> Result<(), SubmitErr> {
    self.sub.notify(self.state)
  }
}

pub struct Handler {
  io: Box<dyn IoHandler>,
  state: *const (),
  store: Arc<OpStore>,
}

unsafe impl Send for Handler {}

impl Handler {
  pub fn new(
    io: Box<dyn IoHandler>,
    store: Arc<OpStore>,
    state: *const (),
  ) -> Self {
    Self { io, store, state }
  }

  pub fn tick(&mut self) -> io::Result<()> {
    let result = self.io.tick(self.state, &self.store)?;
    // assert!(!result.is_empty());

    eprintln!("[Handler::tick] Processing {} completed operations", result.len());

    for one in result {
      eprintln!("[Handler::tick] Setting op_id={} as done with result={}", one.op_id, one.result);
      let exists = self.store.get_mut(one.op_id, |reg| {
        reg.set_done(one.result);
      });
      eprintln!("[Handler::tick] Op {} exists in store: {:?}", one.op_id, exists.is_some());
    }
    Ok(())
  }

  #[cfg(test)]
  pub fn try_tick(&mut self) -> io::Result<()> {
    let result = self.io.try_tick(self.state, &self.store)?;
    // assert!(!result.is_empty());

    for one in result {
      let _exists = self.store.get_mut(one.op_id, |reg| {
        reg.set_done(one.result);
      });
    }

    Ok(())
  }
}
