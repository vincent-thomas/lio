use std::os::fd::RawFd;

#[cfg(linux)]
use io_uring::types::Fd;

use crate::op::DetachSafe;

use crate::op::Operation;

pub struct Fsync {
  fd: RawFd,
}

impl Fsync {
  pub(crate) fn new(fd: RawFd) -> Self {
    Self { fd }
  }
}

unsafe impl DetachSafe for Fsync {}

impl Operation for Fsync {
  impl_result!(());
  impl_no_readyness!();

  #[cfg(linux)]
  const OPCODE: u8 = 3;

  #[cfg(linux)]
  fn create_entry(&mut self) -> io_uring::squeue::Entry {
    io_uring::opcode::Fsync::new(Fd(self.fd)).build()
  }

  fn run_blocking(&self) -> std::io::Result<i32> {
    syscall!(fsync(self.fd))
  }
}
