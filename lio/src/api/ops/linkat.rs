use std::{ffi::CString, os::fd::AsRawFd};

use crate::api::resource::Resource;

use crate::operation::{Operation, OperationExt};

pub struct LinkAt {
  old_dir_res: Resource,
  old_path: CString,
  new_dir_res: Resource,
  new_path: CString,
}

assert_op_max_size!(LinkAt);

// TODO: test
impl LinkAt {
  pub(crate) fn new(
    old_dir_res: Resource,
    old_path: CString,
    new_dir_res: Resource,
    new_path: CString,
  ) -> Self {
    Self { old_dir_res, old_path, new_dir_res, new_path }
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
  fn create_entry(&self) -> lio_uring::submission::Entry {
    lio_uring::operation::LinkAt::new(
      self.old_dir_res.as_raw_fd(),
      self.old_path.as_ptr(),
      self.new_dir_res.as_raw_fd(),
      self.new_path.as_ptr(),
    )
    .build()
  }

  fn run_blocking(&self) -> isize {
    syscall!(raw linkat(
      self.old_dir_res.as_raw_fd(),
      self.old_path.as_ptr(),
      self.new_dir_res.as_raw_fd(),
      self.new_path.as_ptr(),
      0
    ))
  }
}
