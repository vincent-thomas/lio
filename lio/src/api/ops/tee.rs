#[cfg(not(linux))]
use crate::operation::EventType;
use std::io;

use crate::api::op::TypedOp;
use crate::api::resource::Resource;

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

impl TypedOp for Tee {
  crate::impl_io_result!(i32);

  fn into_op(&mut self) -> crate::backend::op::Op {
    crate::backend::op::Op::Tee {
      fd_in: self.res_in.clone(),
      fd_out: self.res_out.clone(),
      size: self.size,
    }
  }
}
