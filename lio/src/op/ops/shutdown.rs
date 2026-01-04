use std::os::fd::AsRawFd;

#[cfg(linux)]
use io_uring::types::Fd;

use crate::resource::Resource;

use crate::op::{Operation, OperationExt};

pub struct Shutdown {
  res: Resource,
  how: i32,
}

assert_op_max_size!(Shutdown);

impl Shutdown {
  pub(crate) fn new(res: Resource, how: i32) -> Self {
    Self { res, how }
  }
}

impl OperationExt for Shutdown {
  type Result = std::io::Result<()>;
}

impl Operation for Shutdown {
  impl_result!(());
  // NOTE: Not sure here, kqueue can prob be used with flags.
  impl_no_readyness!();

  #[cfg(linux)]
  // const OPCODE: u8 = 34;
  #[cfg(linux)]
  fn create_entry(&self) -> io_uring::squeue::Entry {
    io_uring::opcode::Shutdown::new(Fd(self.res.as_raw_fd()), self.how).build()
  }

  fn run_blocking(&self) -> isize {
    syscall_raw!(shutdown(self.res.as_raw_fd(), self.how))
  }
}
