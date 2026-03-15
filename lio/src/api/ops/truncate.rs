use crate::api::op::TypedOp;
use crate::api::resource::Resource;

pub struct Truncate {
  res: Resource,
  size: u64,
}

assert_op_max_size!(Truncate);

impl Truncate {
  pub(crate) fn new(res: Resource, size: u64) -> Self {
    Self { res, size }
  }
}

impl TypedOp for Truncate {
  crate::impl_io_result!();

  fn into_op(&mut self) -> crate::backend::op::Op {
    crate::backend::op::Op::Truncate { fd: self.res.clone(), size: self.size }
  }
}
