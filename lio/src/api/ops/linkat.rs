use std::{ffi::CString, os::fd::AsRawFd};

use crate::api::resource::Resource;

use crate::typed_op::TypedOp;

pub struct LinkAt {
  old_dir_res: Resource,
  old_path: CString,
  new_dir_res: Resource,
  new_path: CString,
}

assert_op_max_size!(LinkAt);

impl LinkAt {
  pub(crate) fn new(
    old_dir_res: Resource,
    old_path: CString,
    new_dir_res: Resource,
    new_path: CString,
  ) -> Self {
    Self { old_dir_res, old_path, new_dir_res, new_path }
  }

  pub fn to_op(self) -> crate::op::Op {
    crate::op::Op::LinkAt {
      old_dir_fd: self.old_dir_res,
      old_path: self.old_path.as_ptr(),
      new_dir_fd: self.new_dir_res,
      new_path: self.new_path.as_ptr(),
    }
  }
}

impl TypedOp for LinkAt {
  type Result = std::io::Result<()>;

  fn into_op(&mut self) -> crate::op::Op {
    crate::op::Op::LinkAt {
      old_dir_fd: self.old_dir_res.clone(),
      old_path: self.old_path.as_ptr(),
      new_dir_fd: self.new_dir_res.clone(),
      new_path: self.new_path.as_ptr(),
    }
  }

  fn extract_result(self, res: isize) -> Self::Result {
    if res < 0 {
      Err(std::io::Error::from_raw_os_error((-res) as i32))
    } else {
      Ok(())
    }
  }

  // #[cfg(unix)]
  // fn meta(&self) -> crate::operation::OpMeta {
  //   crate::operation::OpMeta::CAP_NONE
  // }

  // #[cfg(unix)]
  // fn cap(&self) -> i32 {
  //   -1
  // }

  // #[cfg(linux)]
  // fn create_entry(&self) -> lio_uring::submission::Entry {
  //   lio_uring::operation::LinkAt::new(
  //     self.old_dir_res.as_raw_fd(),
  //     self.old_path.as_ptr(),
  //     self.new_dir_res.as_raw_fd(),
  //     self.new_path.as_ptr(),
  //   )
  //   .build()
  // }

  // fn run_blocking(&self) -> isize {
  //   syscall!(raw linkat(
  //     self.old_dir_res.as_raw_fd(),
  //     self.old_path.as_ptr(),
  //     self.new_dir_res.as_raw_fd(),
  //     self.new_path.as_ptr(),
  //     0
  //   ))
  // }
}
