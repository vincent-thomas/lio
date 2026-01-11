use std::os::fd::AsRawFd;

#[cfg(linux)]
use io_uring::types::Fd;

use crate::api::resource::Resource;

use crate::operation::{Operation, OperationExt};

pub struct Listen {
  res: Resource,
  backlog: i32,
}

assert_op_max_size!(Listen);

impl Listen {
  pub(crate) fn new(res: Resource, backlog: i32) -> Self {
    Self { res, backlog }
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
    io_uring::opcode::Listen::new(Fd(self.res.as_raw_fd()), self.backlog)
      .build()
  }

  fn run_blocking(&self) -> isize {
    syscall!(raw listen(self.res.as_raw_fd(), self.backlog)?)
  }
}
