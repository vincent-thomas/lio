use std::os::fd::AsRawFd;

#[cfg(linux)]
use io_uring::{opcode, squeue, types::Fd};

use crate::op::{DetachSafe, Operation, OperationExt};
use crate::resource::Resource;

pub struct Truncate {
  res: Resource,
  size: u64,
}

assert_op_max_size!(Truncate);

unsafe impl DetachSafe for Truncate {}

impl Truncate {
  pub(crate) fn new(res: Resource, size: u64) -> Self {
    Self { res, size }
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
    opcode::Ftruncate::new(Fd(self.res.as_raw_fd()), self.size).build()
  }

  fn run_blocking(&self) -> isize {
    syscall_raw!(ftruncate(self.res.as_raw_fd(), self.size as i64))
  }
}
