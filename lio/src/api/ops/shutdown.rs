use crate::api::op::TypedOp;
use crate::api::resource::Resource;

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
  crate::impl_io_result!();

  fn into_op(&mut self) -> crate::backend::op::Op {
    crate::backend::op::Op::Shutdown { fd: self.res.clone(), how: self.how }
  }
}
