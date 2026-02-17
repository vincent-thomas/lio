#[cfg(not(linux))]
use crate::operation::EventType;
use std::{io, os::fd::AsRawFd};

use crate::api::resource::Resource;
use crate::typed_op::TypedOp;

pub struct Tee {
  res_in: Resource,
  res_out: Resource,
  size: u32,
}

assert_op_max_size!(Tee);

impl Tee {
  pub(crate) fn new(res_in: Resource, res_out: Resource, size: u32) -> Self {
    Self { res_in, res_out, size }
  }

  pub fn to_op(self) -> crate::op::Op {
    crate::op::Op::Tee {
      fd_in: self.res_in,
      fd_out: self.res_out,
      size: self.size,
    }
  }
}

impl TypedOp for Tee {
  type Result = io::Result<i32>;

  fn into_op(&mut self) -> crate::op::Op {
    crate::op::Op::Tee {
      fd_in: self.res_in.clone(),
      fd_out: self.res_out.clone(),
      size: self.size,
    }
  }

  fn extract_result(self, res: isize) -> Self::Result {
    if res < 0 {
      Err(io::Error::from_raw_os_error((-res) as i32))
    } else {
      Ok(res as i32)
    }
  }

  // #[cfg(unix)]
  // fn meta(&self) -> crate::operation::OpMeta {
  //   crate::operation::OpMeta::CAP_FD
  //     | crate::operation::OpMeta::FD_READ
  //     | crate::operation::OpMeta::FD_WRITE
  // }

  // #[cfg(unix)]
  // fn cap(&self) -> i32 {
  //   self.res_in.as_raw_fd()
  // }

  // #[cfg(linux)]
  // fn create_entry(&self) -> lio_uring::submission::Entry {
  //   lio_uring::operation::Tee::new(
  //     self.res_in.as_raw_fd(),
  //     self.res_out.as_raw_fd(),
  //     self.size,
  //   )
  //   .build()
  // }

  // fn run_blocking(&self) -> isize {
  //   syscall!(raw tee(
  //     self.res_in.as_raw_fd(),
  //     self.res_out.as_raw_fd(),
  //     self.size as usize,
  //     0
  //   ))
  // }
}
