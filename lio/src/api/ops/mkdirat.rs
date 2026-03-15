use std::ffi::CString;

use crate::api::op::TypedOp;
use crate::api::resource::Resource;

/// Operation to create a directory.
pub struct MkdirAt {
  dir_res: Resource,
  path: CString,
  mode: u32,
}

assert_op_max_size!(MkdirAt);

impl MkdirAt {
  pub(crate) fn new(dir_res: Resource, path: CString, mode: u32) -> Self {
    Self { dir_res, path, mode }
  }
}

impl TypedOp for MkdirAt {
  crate::impl_io_result!();

  fn into_op(&mut self) -> crate::backend::op::Op {
    crate::backend::op::Op::MkdirAt {
      dir_fd: self.dir_res.clone(),
      path: self.path.as_ptr(),
      mode: self.mode,
    }
  }
}
