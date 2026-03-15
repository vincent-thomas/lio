use std::ffi::CString;

use crate::api::op::TypedOp;
use crate::api::resource::Resource;

/// Operation to rename a file or directory.
pub struct RenameAt {
  old_dir_res: Resource,
  old_path: CString,
  new_dir_res: Resource,
  new_path: CString,
}

assert_op_max_size!(RenameAt);

impl RenameAt {
  pub(crate) fn new(
    old_dir_res: Resource,
    old_path: CString,
    new_dir_res: Resource,
    new_path: CString,
  ) -> Self {
    Self { old_dir_res, old_path, new_dir_res, new_path }
  }
}

impl TypedOp for RenameAt {
  crate::impl_io_result!();

  fn into_op(&mut self) -> crate::backend::op::Op {
    crate::backend::op::Op::RenameAt {
      old_dir_fd: self.old_dir_res.clone(),
      old_path: self.old_path.as_ptr(),
      new_dir_fd: self.new_dir_res.clone(),
      new_path: self.new_path.as_ptr(),
    }
  }
}
