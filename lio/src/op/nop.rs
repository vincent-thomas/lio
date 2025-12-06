use crate::op::Operation;
use std::io;

pub struct Nop;

impl Operation for Nop {
  #[cfg(unix)]
  const OPCODE: u8 = 0;

  #[cfg(unix)]
  type Result = ();
  /// File descriptor returned from the operation.
  fn result(&mut self, _out: std::io::Result<i32>) -> Self::Result {
    ()
  }

  #[cfg(linux)]
  fn create_entry(&mut self) -> io_uring::squeue::Entry {
    io_uring::opcode::Nop::new().build()
  }

  fn fd(&self) -> Option<std::os::fd::RawFd> {
    None
  }

  fn run_blocking(&self) -> io::Result<i32> {
    panic!();
  }
}
