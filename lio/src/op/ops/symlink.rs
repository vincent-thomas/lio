use std::{ffi::CString, os::fd::RawFd};

#[cfg(linux)]
use io_uring::types::Fd;

use crate::op::{Operation, OperationExt};

pub struct SymlinkAt {
  fd: RawFd,
  target: CString,
  linkpath: CString,
}

assert_op_max_size!(SymlinkAt);

// Not detach safe.

// TODO: test
impl SymlinkAt {
  pub(crate) fn new(
    new_dir_fd: RawFd,
    target: CString,
    linkpath: CString,
  ) -> Self {
    Self { fd: new_dir_fd, target, linkpath }
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
      Fd(self.fd),
      self.target.as_ptr(),
      self.linkpath.as_ptr(),
    )
    .build()
  }

  fn run_blocking(&self) -> std::io::Result<i32> {
    syscall!(symlinkat(self.target.as_ptr(), self.fd, self.linkpath.as_ptr()))
  }
}
