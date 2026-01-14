use std::os::fd::AsRawFd;

use crate::api::resource::Resource;

use crate::operation::{Operation, OperationExt};

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
  fn create_entry(&self) -> lio_uring::submission::Entry {
    lio_uring::operation::Shutdown::new(self.res.as_raw_fd(), self.how).build()
  }

  fn run_blocking(&self) -> isize {
    syscall!(raw shutdown(self.res.as_raw_fd(), self.how))
  }
}
