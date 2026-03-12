use crate::api::resource::Resource;

use crate::typed_op::TypedOp;

pub struct Listen {
  res: Resource,
  backlog: i32,
}

assert_op_max_size!(Listen);

impl Listen {
  pub(crate) fn new(res: Resource, backlog: i32) -> Self {
    Self { res, backlog }
  }

  pub fn to_op(self) -> crate::op::Op {
    crate::op::Op::Listen { fd: self.res, backlog: self.backlog }
  }
}

impl TypedOp for Listen {
  type Result = std::io::Result<()>;

  fn into_op(&mut self) -> crate::op::Op {
    crate::op::Op::Listen { fd: self.res.clone(), backlog: self.backlog }
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
  //   lio_uring::operation::Listen::new(self.res.as_raw_fd(), self.backlog)
  //     .build()
  // }

  // fn run_blocking(&self) -> isize {
  //   syscall!(raw listen(self.res.as_raw_fd(), self.backlog)?)
  // }
}
