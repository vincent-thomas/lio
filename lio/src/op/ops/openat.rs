use std::{ffi::CString, os::fd::AsRawFd};

#[cfg(linux)]
use io_uring::types::Fd;

use crate::op::{Operation, OperationExt};
use crate::resource::Resource;

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
  fn create_entry(&self) -> io_uring::squeue::Entry {
    io_uring::opcode::OpenAt::new(
      Fd(self.dir_res.as_raw_fd()),
      self.pathname.as_ptr(),
    )
    .flags(self.flags)
    .build()
  }
  fn run_blocking(&self) -> isize {
    syscall_raw!(openat(
      self.dir_res.as_raw_fd(),
      self.pathname.as_ptr(),
      self.flags
    ))
  }
}
