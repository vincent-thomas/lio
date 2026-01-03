use std::io;
use std::os::fd::AsRawFd;

#[cfg(linux)]
use io_uring::{opcode, types::Fd};

use crate::op::DetachSafe;
use crate::resource::Resource;

use crate::op::Operation;
use crate::op::OperationExt;

pub struct Close {
  res: Resource,
}

unsafe impl DetachSafe for Close {}

impl Close {
  pub(crate) fn new(res: Resource) -> Self {
    Self { res }
  }
}

assert_op_max_size!(Close);

impl OperationExt for Close {
  type Result = io::Result<()>;
}

impl Operation for Close {
  impl_result!(());
  impl_no_readyness!();

  // #[cfg(linux)]
  // const OPCODE: u8 = 19;

  #[cfg(linux)]
  fn create_entry(&self) -> io_uring::squeue::Entry {
    opcode::Close::new(Fd(self.res.as_raw_fd())).build()
  }

  fn run_blocking(&self) -> isize {
    syscall_raw!(close(self.res.as_raw_fd()))
  }
}
