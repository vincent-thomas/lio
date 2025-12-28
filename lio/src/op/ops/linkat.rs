use std::{ffi::CString, os::fd::RawFd};

#[cfg(linux)]
use io_uring::types::Fd;

use crate::op::DetachSafe;

use crate::op::{Operation, OperationExt};

pub struct LinkAt {
  old_dir_fd: RawFd,
  old_path: CString,
  new_dir_fd: RawFd,
  new_path: CString,
}

assert_op_max_size!(LinkAt);

unsafe impl DetachSafe for LinkAt {}

// TODO: test
impl LinkAt {
  pub(crate) fn new(
    old_dir_fd: RawFd,
    old_path: CString,
    new_dir_fd: RawFd,
    new_path: CString,
  ) -> Self {
    Self { old_dir_fd, old_path, new_dir_fd, new_path }
  }
}

impl OperationExt for LinkAt {
  type Result = std::io::Result<()>;
}

impl Operation for LinkAt {
  impl_result!(());
  impl_no_readyness!();

  #[cfg(linux)]
  // const OPCODE: u8 = 39;
  #[cfg(linux)]
  fn create_entry(&self) -> io_uring::squeue::Entry {
    io_uring::opcode::LinkAt::new(
      Fd(self.old_dir_fd),
      self.old_path.as_ptr(),
      Fd(self.new_dir_fd),
      self.new_path.as_ptr(),
    )
    .build()
  }

  fn run_blocking(&self) -> std::io::Result<i32> {
    syscall!(linkat(
      self.old_dir_fd,
      self.old_path.as_ptr(),
      self.new_dir_fd,
      self.new_path.as_ptr(),
      0
    ))
  }
}
