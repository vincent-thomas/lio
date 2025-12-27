use std::io;
use std::os::fd::RawFd;

#[cfg(linux)]
use io_uring::{opcode, types::Fd};

use crate::op::DetachSafe;

use crate::op::Operation;
use crate::op::OperationExt;

pub struct Close {
  fd: RawFd,
}

unsafe impl DetachSafe for Close {}

impl Close {
  pub(crate) fn new(fd: RawFd) -> Self {
    Self { fd }
  }
}

impl OperationExt for Close {
  type Result = io::Result<()>;
}

impl Operation for Close {
  impl_result!(());
  impl_no_readyness!();

  // #[cfg(linux)]
  // const OPCODE: u8 = 19;

  #[cfg(linux)]
  fn create_entry(&mut self) -> io_uring::squeue::Entry {
    opcode::Close::new(Fd(self.fd)).build()
  }

  fn run_blocking(&self) -> std::io::Result<i32> {
    syscall!(close(self.fd))
  }
}
