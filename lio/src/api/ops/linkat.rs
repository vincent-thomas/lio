use std::ffi::CString;

use crate::api::resource::Resource;

use crate::api::op::TypedOp;

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
}

impl TypedOp for LinkAt {
  crate::impl_io_result!();

  fn into_op(&mut self) -> crate::backend::op::Op {
    crate::backend::op::Op::LinkAt {
      old_dir_fd: self.old_dir_res.clone(),
      old_path: self.old_path.as_ptr(),
      new_dir_fd: self.new_dir_res.clone(),
      new_path: self.new_path.as_ptr(),
    }
  }
}
