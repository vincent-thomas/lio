use std::{ffi::CString, os::fd::FromRawFd};

use crate::api::op::TypedOp;
use crate::api::resource::Resource;

pub struct OpenAt {
  dir_res: Resource,
  pathname: CString,
  flags: i32,
  mode: u32,
}

assert_op_max_size!(OpenAt);

impl OpenAt {
  /// Creates a new OpenAt operation with default mode (0o666).
  pub(crate) fn new(dir_res: Resource, pathname: CString, flags: i32) -> Self {
    Self { dir_res, pathname, flags, mode: 0o666 }
  }

  /// Creates a new OpenAt operation with explicit mode.
  pub(crate) fn with_mode(
    dir_res: Resource,
    pathname: CString,
    flags: i32,
    mode: u32,
  ) -> Self {
    Self { dir_res, pathname, flags, mode }
  }
}

impl TypedOp for OpenAt {
  crate::impl_io_result!(Resource);

  fn into_op(&mut self) -> crate::backend::op::Op {
    crate::backend::op::Op::OpenAt {
      dir_fd: self.dir_res.clone(),
      path: self.pathname.as_ptr(),
      flags: self.flags,
      mode: self.mode,
    }
  }
}
