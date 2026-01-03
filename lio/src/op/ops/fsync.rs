use std::os::fd::AsRawFd;

#[cfg(linux)]
use io_uring::types::Fd;

use crate::op::DetachSafe;
use crate::resource::Resource;

use crate::op::{Operation, OperationExt};

pub struct Fsync {
  res: Resource,
}
assert_op_max_size!(Fsync);

impl Fsync {
  pub(crate) fn new(res: Resource) -> Self {
    Self { res }
  }
}

unsafe impl DetachSafe for Fsync {}

impl OperationExt for Fsync {
  type Result = std::io::Result<()>;
}

impl Operation for Fsync {
  impl_result!(());
  impl_no_readyness!();

  #[cfg(linux)]
  // const OPCODE: u8 = 3;
  #[cfg(linux)]
  fn create_entry(&self) -> io_uring::squeue::Entry {
    io_uring::opcode::Fsync::new(Fd(self.res.as_raw_fd())).build()
  }

  fn run_blocking(&self) -> isize {
    syscall_raw!(fsync(self.res.as_raw_fd()))
  }
}
