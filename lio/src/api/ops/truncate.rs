use std::os::fd::AsRawFd;

use crate::api::resource::Resource;
use crate::operation::{Operation, OperationExt};

pub struct Truncate {
  res: Resource,
  size: u64,
}

assert_op_max_size!(Truncate);

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
  fn create_entry(&self) -> lio_uring::submission::Entry {
    lio_uring::operation::Ftruncate::new(self.res.as_raw_fd(), self.size).build()
  }

  fn run_blocking(&self) -> isize {
    syscall!(raw ftruncate(self.res.as_raw_fd(), self.size as i64))
  }
}
