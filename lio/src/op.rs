macro_rules! syscall {
  ($fn: ident ( $($arg: expr),* $(,)* ) ) => {{
      #[allow(unused_unsafe)]
      let res = unsafe { libc::$fn($($arg, )*) };
      if res == -1 {
          Err(std::io::Error::last_os_error())
      } else {
          Ok(res)
      }
  }};
}

use std::io;
use std::os::fd::RawFd;

mod accept;
mod bind;
mod close;
mod connect;
mod listen;
pub(crate) mod net_utils;
mod openat;
mod read;
mod recv;
mod send;
mod socket;

mod fsync;
mod linkat;
#[cfg(linux)]
pub mod nop;
mod shutdown;
mod symlink;
#[cfg(linux)]
mod tee;
#[cfg(linux)]
mod timeout;
mod truncate;
mod write;

pub use accept::*;
pub use bind::*;
pub use close::*;
pub use connect::*;
pub use fsync::*;
pub use linkat::*;
pub use listen::*;
#[cfg(linux)]
#[allow(unused_imports)]
pub(crate) use nop::*;
pub use openat::*;
pub use read::*;
pub use recv::*;
pub use send::*;
pub use shutdown::*;
pub use socket::*;
pub use symlink::*;
#[cfg(linux)]
pub use timeout::*;

#[cfg(linux)]
pub use tee::*;

pub use truncate::*;
pub use write::*;

/// Done to disallow someone creating a operation outside of lio, which will cause issues.
trait Sealed {}
impl<O: Operation> Sealed for O {}

// Safety: Decides if resources can be leaked when using OperationProgress::detach
#[allow(private_bounds)]
pub unsafe trait DetachSafe: Sealed {}

// Things that implement this trait represent a command that can be executed using io-uring.
#[allow(private_bounds)]
pub trait Operation: Sealed {
  type Result;
  /// This is guarranteed to fire after this has completed and only fire ONCE.
  /// i32 is guarranteed to be >= 0.
  fn result(&mut self, _ret: io::Result<i32>) -> Self::Result;

  #[cfg(linux)]
  const OPCODE: u8;

  #[cfg(linux)]
  fn entry_supported(probe: &io_uring::Probe) -> bool {
    probe.is_supported(Self::OPCODE)
  }

  #[cfg(linux)]
  fn create_entry(&mut self) -> io_uring::squeue::Entry;

  const IS_CONNECT: bool = false;

  const EVENT_TYPE: Option<EventType> = None;

  fn fd(&self) -> Option<RawFd>;

  fn run_blocking(&self) -> io::Result<i32>;
}

#[derive(Debug)]
pub enum EventType {
  Read,
  Write,
}
