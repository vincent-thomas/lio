use crate::typed_op::TypedOp;
use std::io;

pub struct Nop;

assert_op_max_size!(Nop);

impl Nop {
  pub fn to_op(self) -> crate::op::Op {
    crate::op::Op::Nop
  }
}

impl TypedOp for Nop {
  type Result = io::Result<()>;

  fn into_op(&mut self) -> crate::op::Op {
    crate::op::Op::Nop
  }

  fn extract_result(self, res: isize) -> Self::Result {
    if res < 0 {
      Err(io::Error::from_raw_os_error((-res) as i32))
    } else {
      Ok(())
    }
  }

  // #[cfg(unix)]
  // fn meta(&self) -> OpMeta {
  //   OpMeta::CAP_NONE
  // }

  // #[cfg(unix)]
  // fn cap(&self) -> i32 {
  //   -1
  // }

  // #[cfg(linux)]
  // fn create_entry(&self) -> lio_uring::submission::Entry {
  //   lio_uring::operation::Nop::new().build()
  // }

  // fn run_blocking(&self) -> isize {
  //   0
  // }
}
