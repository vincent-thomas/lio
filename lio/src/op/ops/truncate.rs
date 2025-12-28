use std::os::fd::RawFd;

#[cfg(linux)]
use io_uring::{opcode, squeue, types::Fd};

use crate::op::{DetachSafe, Operation, OperationExt};

pub struct Truncate {
  fd: RawFd,
  size: u64,
}

assert_op_max_size!(Truncate);

unsafe impl DetachSafe for Truncate {}

impl Truncate {
  pub(crate) fn new(fd: RawFd, size: u64) -> Self {
    Self { fd, size }
  }
}

impl OperationExt for Truncate {
  type Result = std::io::Result<()>;
}

impl Operation for Truncate {
  impl_result!(());
  impl_no_readyness!();

  #[cfg(linux)]
  // const OPCODE: u8 = 55;
  #[cfg(linux)]
  fn create_entry(&self) -> squeue::Entry {
    opcode::Ftruncate::new(Fd(self.fd), self.size).build()
  }

  fn run_blocking(&self) -> std::io::Result<i32> {
    syscall!(ftruncate(self.fd, self.size as i64))
  }
}
