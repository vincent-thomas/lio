use crate::api::resource::Resource;

use crate::api::op::TypedOp;

pub struct Listen {
  res: Resource,
  backlog: i32,
}

assert_op_max_size!(Listen);

impl Listen {
  pub(crate) fn new(res: Resource, backlog: i32) -> Self {
    Self { res, backlog }
  }
}

impl TypedOp for Listen {
  crate::impl_io_result!();

  fn into_op(&mut self) -> crate::backend::op::Op {
    crate::backend::op::Op::Listen {
      fd: self.res.clone(),
      backlog: self.backlog,
    }
  }
}
