use std::{ffi::CString, os::fd::AsRawFd};

#[cfg(linux)]
use io_uring::types::Fd;

use crate::operation::{Operation, OperationExt};
use crate::api::resource::Resource;

pub struct SymlinkAt {
  dir_res: Resource,
  target: CString,
  linkpath: CString,
}

assert_op_max_size!(SymlinkAt);

// Not detach safe.

// TODO: test
impl SymlinkAt {
  pub(crate) fn new(
    dir_res: Resource,
    target: CString,
    linkpath: CString,
  ) -> Self {
    Self { dir_res, target, linkpath }
  }
}

impl OperationExt for SymlinkAt {
  type Result = std::io::Result<()>;
}

impl Operation for SymlinkAt {
  impl_result!(());
  impl_no_readyness!();

  #[cfg(linux)]
  // const OPCODE: u8 = 38;
  #[cfg(linux)]
  fn create_entry(&self) -> io_uring::squeue::Entry {
    io_uring::opcode::SymlinkAt::new(
      Fd(self.dir_res.as_raw_fd()),
      self.target.as_ptr(),
      self.linkpath.as_ptr(),
    )
    .build()
  }

  fn run_blocking(&self) -> isize {
    syscall!(raw symlinkat(
      self.target.as_ptr(),
      self.dir_res.as_raw_fd(),
      self.linkpath.as_ptr()
    ))
  }
}
