use crate::api::resource::Resource;
use crate::typed_op::TypedOp;

pub struct Shutdown {
  res: Resource,
  how: i32,
}

assert_op_max_size!(Shutdown);

impl Shutdown {
  pub(crate) fn new(res: Resource, how: i32) -> Self {
    Self { res, how }
  }
}

impl TypedOp for Shutdown {
  type Result = std::io::Result<()>;

  fn into_op(&mut self) -> crate::op::Op {
    crate::op::Op::Shutdown { fd: self.res.clone(), how: self.how }
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
  //   crate::operation::OpMeta::CAP_FD
  // }

  // #[cfg(unix)]
  // fn cap(&self) -> i32 {
  //   self.res.as_raw_fd()
  // }

  // fn run_blocking(&self) -> isize {
  //   use std::os::fd::AsRawFd;
  //   unsafe { libc::shutdown(self.res.as_raw_fd(), self.how) as isize }
  // }
}
