use std::ffi::CString;

use crate::api::op::TypedOp;
use crate::api::resource::Resource;

/// Operation to remove a file or directory.
pub struct UnlinkAt {
  dir_res: Resource,
  path: CString,
  flags: i32,
}

assert_op_max_size!(UnlinkAt);

impl UnlinkAt {
  pub(crate) fn new(dir_res: Resource, path: CString, flags: i32) -> Self {
    Self { dir_res, path, flags }
  }
}

impl TypedOp for UnlinkAt {
  crate::impl_io_result!();

  fn into_op(&mut self) -> crate::backend::op::Op {
    crate::backend::op::Op::UnlinkAt {
      dir_fd: self.dir_res.clone(),
      path: self.path.as_ptr(),
      flags: self.flags,
    }
  }
}
