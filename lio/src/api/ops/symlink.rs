use std::ffi::CString;

use crate::api::op::TypedOp;
use crate::api::resource::Resource;

pub struct SymlinkAt {
  dir_res: Resource,
  target: CString,
  linkpath: CString,
}

assert_op_max_size!(SymlinkAt);

impl SymlinkAt {
  pub(crate) fn new(
    dir_res: Resource,
    target: CString,
    linkpath: CString,
  ) -> Self {
    Self { dir_res, target, linkpath }
  }
}

impl TypedOp for SymlinkAt {
  crate::impl_io_result!();

  fn into_op(&mut self) -> crate::backend::op::Op {
    crate::backend::op::Op::SymlinkAt {
      dir_fd: self.dir_res.clone(),
      target: self.target.as_ptr(),
      linkpath: self.linkpath.as_ptr(),
    }
  }
}
