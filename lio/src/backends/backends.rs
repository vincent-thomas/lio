#[cfg(test)]
#[macro_use]
pub mod test_macro;

pub use impls::*;
mod impls {

  #[cfg(test)]
  pub mod dummy;

  // #[cfg(test)]
  // pub use dummy::DummyDriver;

  #[cfg(linux)]
  pub mod io_uring;
  // #[cfg(linux)]
  // pub use self::io_uring::{
  //   IoUring, IoUringHandler, IoUringState, IoUringSubmitter,
  // };

  pub mod pollingv2;

  // pub use pollingv2::{Poller, PollerHandle, PollerState, PollerSubmitter};

  #[cfg(windows)]
  mod iocp;
  #[cfg(windows)]
  pub use iocp::*;
}

mod handler;
pub use handler::*;

mod submitter;
pub use submitter::*;

use std::io;

// use crate::registration::StoredOp;

#[derive(Debug)]
pub enum SubmitErr {
  Io(io::Error),
  Full,
  NotCompatible,
  DriverShutdown,
}

impl From<io::Error> for SubmitErr {
  fn from(value: io::Error) -> Self {
    Self::Io(value)
  }
}

pub trait IoDriver {
  type Submitter: IoSubmitter + 'static;
  type Handler: IoHandler + 'static;

  fn new_state() -> io::Result<*const ()>;
  fn drop_state(state: *const ());

  fn new(state: *const ()) -> io::Result<(Self::Submitter, Self::Handler)>;
}
