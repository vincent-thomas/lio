use std::{ffi::CString, os::fd::FromRawFd};

use crate::api::resource::Resource;
use crate::typed_op::TypedOp;

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

  pub fn to_op(self) -> crate::op::Op {
    crate::op::Op::OpenAt {
      dir_fd: self.dir_res,
      path: self.pathname.as_ptr(),
      flags: self.flags,
    }
  }
}

impl TypedOp for OpenAt {
  type Result = std::io::Result<Resource>;

  fn into_op(&mut self) -> crate::op::Op {
    crate::op::Op::OpenAt {
      dir_fd: self.dir_res.clone(),
      path: self.pathname.as_ptr(),
      flags: self.flags,
    }
  }

  fn extract_result(self, res: isize) -> Self::Result {
    if res < 0 {
      Err(std::io::Error::from_raw_os_error((-res) as i32))
    } else {
      // SAFETY: 'res' is valid fd.
      Ok(unsafe { Resource::from_raw_fd(res as i32) })
    }
  }

  // #[cfg(unix)]
  // fn meta(&self) -> crate::operation::OpMeta {
  //   crate::operation::OpMeta::CAP_FD
  // }

  // #[cfg(unix)]
  // fn cap(&self) -> i32 {
  //   self.dir_res.as_raw_fd()
  // }

  // #[cfg(linux)]
  // fn create_entry(&self) -> lio_uring::submission::Entry {
  //   lio_uring::operation::OpenAt::new(
  //     self.dir_res.as_raw_fd(),
  //     self.pathname.as_ptr(),
  //   )
  //   .flags(self.flags)
  //   .build()
  // }

  // fn run_blocking(&self) -> isize {
  //   syscall!(raw openat(
  //     self.dir_res.as_raw_fd(),
  //     self.pathname.as_ptr(),
  //     self.flags
  //   ))
  // }
}
