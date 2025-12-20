pub(crate) mod net_utils;

mod ops;
mod progress;
pub use ops::*;
pub use progress::*;
use std::io;
use std::ops::BitOr;
use std::os::fd::RawFd;

/// Operation flags using bitflags pattern
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpFlags(u8);

impl OpFlags {
  pub const NONE: Self = Self(0);
  pub const IS_CONNECT: Self = Self(1 << 0);

  /// Check if the IS_CONNECT flag is set
  pub const fn is_connect(self) -> bool {
    self.0 & Self::IS_CONNECT.0 != 0
  }
}

impl BitOr for OpFlags {
  type Output = Self;

  fn bitor(self, rhs: Self) -> Self::Output {
    Self(self.0 | rhs.0)
  }
}

trait Sealed {}
/// Done to disallow someone creating a operation outside of lio, which will cause issues.
impl<O: Operation> Sealed for O {}

// Safety: Decides if resources can be leaked when using OperationProgress::detach
#[allow(private_bounds)]
pub unsafe trait DetachSafe: Sealed {}

// Things that implement this trait represent a command that can be executed using io-uring.
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

  const FLAGS: OpFlags = OpFlags::NONE;

  #[cfg(unix)]
  const INTEREST: Option<crate::backends::pollingv2::Interest> = None;

  #[cfg(unix)]
  fn fd(&self) -> Option<RawFd>;

  fn run_blocking(&self) -> io::Result<i32>;
}
