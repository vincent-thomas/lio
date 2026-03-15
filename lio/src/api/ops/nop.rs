use crate::api::op::TypedOp;
use std::io;

pub struct Nop;

assert_op_max_size!(Nop);

impl TypedOp for Nop {
  crate::impl_io_result!();

  fn into_op(&mut self) -> crate::backend::op::Op {
    crate::backend::op::Op::Nop
  }
}
