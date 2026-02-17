use std::os::fd::AsRawFd;

use crate::api::resource::Resource;

use crate::typed_op::TypedOp;

pub struct Fsync {
  res: Resource,
}
assert_op_max_size!(Fsync);

impl Fsync {
  pub(crate) fn new(res: Resource) -> Self {
    Self { res }
  }

  pub fn to_op(self) -> crate::op::Op {
    crate::op::Op::Fsync { fd: self.res }
  }
}

impl TypedOp for Fsync {
  type Result = std::io::Result<()>;

  fn into_op(&mut self) -> crate::op::Op {
    crate::op::Op::Fsync { fd: self.res.clone() }
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

  // #[cfg(linux)]
  // fn create_entry(&self) -> lio_uring::submission::Entry {
  //   lio_uring::operation::Fsync::new(self.res.as_raw_fd()).build()
  // }

  // fn run_blocking(&self) -> isize {
  //   syscall!(raw fsync(self.res.as_raw_fd()))
  // }
}
