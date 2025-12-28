use std::os::fd::RawFd;

#[cfg(linux)]
use io_uring::types::Fd;

use crate::op::DetachSafe;

use crate::op::{Operation, OperationExt};

pub struct Listen {
  fd: RawFd,
  backlog: i32,
}

assert_op_max_size!(Listen);

unsafe impl DetachSafe for Listen {}

impl Listen {
  pub(crate) fn new(fd: RawFd, backlog: i32) -> Self {
    Self { fd, backlog }
  }
}

impl OperationExt for Listen {
  type Result = std::io::Result<()>;
}

impl Operation for Listen {
  impl_result!(());
  impl_no_readyness!();

  #[cfg(linux)]
  // const OPCODE: u8 = 57;
  #[cfg(linux)]
  fn create_entry(&self) -> io_uring::squeue::Entry {
    io_uring::opcode::Listen::new(Fd(self.fd), self.backlog).build()
  }

  fn run_blocking(&self) -> std::io::Result<i32> {
    syscall!(listen(self.fd, self.backlog))
  }
}
