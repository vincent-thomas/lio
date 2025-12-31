use crate::op::{OpMeta, Operation, OperationExt};

pub struct Nop;

assert_op_max_size!(Nop);

impl OperationExt for Nop {
  type Result = ();
}

impl Operation for Nop {
  impl_result!(());

  // #[cfg(unix)]
  // const OPCODE: u8 = 0;

  fn meta(&self) -> OpMeta {
    OpMeta::CAP_NONE
  }

  #[cfg(linux)]
  fn create_entry(&self) -> io_uring::squeue::Entry {
    io_uring::opcode::Nop::new().build()
  }

  #[cfg(unix)]
  fn run_blocking(&self) -> isize {
    0
  }
}
