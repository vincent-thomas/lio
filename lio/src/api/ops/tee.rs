#[cfg(not(linux))]
use crate::operation::EventType;
use std::{io, os::fd::AsRawFd};

use crate::api::resource::Resource;
use crate::operation::{Operation, OperationExt};

// TODO: not sure detach safe.
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
}

impl OperationExt for Tee {
  type Result = io::Result<i32>;
}

impl Operation for Tee {
  impl_result!(|_this, ret: isize| -> io::Result<i32> {
    if ret < 0 {
      Err(io::Error::from_raw_os_error((-ret) as i32))
    } else {
      Ok(ret as i32)
    }
  });

  impl_no_readyness!();

  #[cfg(linux)]
  // const OPCODE: u8 = 33;
  #[cfg(linux)]
  fn create_entry(&self) -> lio_uring::submission::Entry {
    lio_uring::operation::Tee::new(
      self.res_in.as_raw_fd(),
      self.res_out.as_raw_fd(),
      self.size,
    )
    .build()
  }

  fn run_blocking(&self) -> isize {
    syscall!(raw tee(
      self.res_in.as_raw_fd(),
      self.res_out.as_raw_fd(),
      self.size as usize,
      0
    ))
  }
}
