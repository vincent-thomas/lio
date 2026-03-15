use crate::api::resource::Resource;

use crate::api::op::TypedOp;

pub struct Fsync {
  res: Resource,
}
assert_op_max_size!(Fsync);

impl Fsync {
  pub(crate) fn new(res: Resource) -> Self {
    Self { res }
  }
}

impl TypedOp for Fsync {
  crate::impl_io_result!();

  fn into_op(&mut self) -> crate::backend::op::Op {
    crate::backend::op::Op::Fsync { fd: self.res.clone() }
  }
}
