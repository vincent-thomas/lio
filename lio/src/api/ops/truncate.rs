use crate::api::resource::Resource;
use crate::typed_op::TypedOp;

pub struct Truncate {
  res: Resource,
  size: u64,
}

assert_op_max_size!(Truncate);

impl Truncate {
  pub(crate) fn new(res: Resource, size: u64) -> Self {
    Self { res, size }
  }

  pub fn to_op(self) -> crate::op::Op {
    crate::op::Op::Truncate { fd: self.res, size: self.size }
  }
}

impl TypedOp for Truncate {
  type Result = std::io::Result<()>;

  fn into_op(&mut self) -> crate::op::Op {
    crate::op::Op::Truncate { fd: self.res.clone(), size: self.size }
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
  //   lio_uring::operation::Ftruncate::new(self.res.as_raw_fd(), self.size)
  //     .build()
  // }

  // fn run_blocking(&self) -> isize {
  //   syscall!(raw ftruncate(self.res.as_raw_fd(), self.size as i64))
  // }
}
