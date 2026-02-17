use std::io;

use crate::{api::resource::Resource, typed_op::TypedOp};

pub struct Close {
  res: Resource,
}

impl Close {
  pub(crate) fn new(res: Resource) -> Self {
    Self { res }
  }
}

assert_op_max_size!(Close);

impl TypedOp for Close {
  type Result = io::Result<()>;

  fn into_op(&mut self) -> crate::op::Op {
    crate::op::Op::Close { fd: self.res.clone() }
  }

  fn extract_result(self, res: isize) -> Self::Result {
    if res < 0 {
      Err(io::Error::from_raw_os_error((-res) as i32))
    } else {
      Ok(())
    }
  }

  // #[cfg(unix)]
  // fn meta(&self) -> crate::operation::OpMeta {
  //   crate::op::Op::Close { fd: self.res.clone() }.meta()
  // }

  // #[cfg(unix)]
  // fn cap(&self) -> i32 {
  //   self.res.as_raw_fd()
  // }

  // fn run_blocking(&self) -> isize {
  //   use std::os::fd::AsRawFd;
  //   unsafe { libc::close(self.res.as_raw_fd()) as isize }
  // }
}
