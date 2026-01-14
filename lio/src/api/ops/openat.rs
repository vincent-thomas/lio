use std::{ffi::CString, os::fd::AsRawFd};

use crate::operation::{Operation, OperationExt};
use crate::api::resource::Resource;

pub struct OpenAt {
  dir_res: Resource,
  pathname: CString,
  flags: i32,
}

assert_op_max_size!(OpenAt);

impl OpenAt {
  pub(crate) fn new(dir_res: Resource, pathname: CString, flags: i32) -> Self {
    Self { dir_res, pathname, flags }
  }
}

impl OperationExt for OpenAt {
  type Result = std::io::Result<Resource>;
}

impl Operation for OpenAt {
  impl_result!(res);
  impl_no_readyness!();

  #[cfg(linux)]
  // const OPCODE: u8 = 18;
  #[cfg(linux)]
  fn create_entry(&self) -> lio_uring::submission::Entry {
    lio_uring::operation::OpenAt::new(
      self.dir_res.as_raw_fd(),
      self.pathname.as_ptr(),
    )
    .flags(self.flags)
    .build()
  }
  fn run_blocking(&self) -> isize {
    syscall!(raw openat(
      self.dir_res.as_raw_fd(),
      self.pathname.as_ptr(),
      self.flags
    ))
  }
}
