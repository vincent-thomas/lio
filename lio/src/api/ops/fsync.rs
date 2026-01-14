use std::os::fd::AsRawFd;

use crate::api::resource::Resource;

use crate::operation::{Operation, OperationExt};

pub struct Fsync {
  res: Resource,
}
assert_op_max_size!(Fsync);

impl Fsync {
  pub(crate) fn new(res: Resource) -> Self {
    Self { res }
  }
}

impl OperationExt for Fsync {
  type Result = std::io::Result<()>;
}

impl Operation for Fsync {
  impl_result!(());
  impl_no_readyness!();

  #[cfg(linux)]
  // const OPCODE: u8 = 3;
  #[cfg(linux)]
  fn create_entry(&self) -> lio_uring::submission::Entry {
    lio_uring::operation::Fsync::new(self.res.as_raw_fd()).build()
  }

  fn run_blocking(&self) -> isize {
    syscall!(raw fsync(self.res.as_raw_fd()))
  }
}
