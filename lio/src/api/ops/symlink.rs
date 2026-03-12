use std::ffi::CString;

use crate::api::resource::Resource;
use crate::typed_op::TypedOp;

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

  pub fn to_op(self) -> crate::op::Op {
    crate::op::Op::SymlinkAt {
      dir_fd: self.dir_res,
      target: self.target.as_ptr(),
      linkpath: self.linkpath.as_ptr(),
    }
  }
}

impl TypedOp for SymlinkAt {
  type Result = std::io::Result<()>;

  fn into_op(&mut self) -> crate::op::Op {
    crate::op::Op::SymlinkAt {
      dir_fd: self.dir_res.clone(),
      target: self.target.as_ptr(),
      linkpath: self.linkpath.as_ptr(),
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
  //   lio_uring::operation::SymlinkAt::new(
  //     self.dir_res.as_raw_fd(),
  //     self.target.as_ptr(),
  //     self.linkpath.as_ptr(),
  //   )
  //   .build()
  // }

  // fn run_blocking(&self) -> isize {
  //   syscall!(raw symlinkat(
  //     self.target.as_ptr(),
  //     self.dir_res.as_raw_fd(),
  //     self.linkpath.as_ptr()
  //   ))
  // }
}
