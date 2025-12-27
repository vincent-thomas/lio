#[cfg(not(linux))]
use crate::op::EventType;
use std::{io, os::fd::RawFd};

use io_uring::types::Fd;

use crate::op::{Operation, OperationExt};

// TODO: not sure detach safe.
pub struct Tee {
  fd_in: RawFd,
  fd_out: RawFd,
  size: u32,
}

impl Tee {
  pub(crate) fn new(fd_in: RawFd, fd_out: RawFd, size: u32) -> Self {
    Self { fd_in, fd_out, size }
  }
}

impl OperationExt for Tee {
  type Result = io::Result<i32>;
}

impl Operation for Tee {
  impl_result!(|_this, ret: io::Result<i32>| -> io::Result<i32> {
    ret
  });

  impl_no_readyness!();

  #[cfg(linux)]
  const OPCODE: u8 = 33;

  #[cfg(linux)]
  fn create_entry(&mut self) -> io_uring::squeue::Entry {
    io_uring::opcode::Tee::new(Fd(self.fd_in), Fd(self.fd_out), self.size)
      .build()
  }

  fn run_blocking(&self) -> std::io::Result<i32> {
    syscall!(tee(self.fd_in, self.fd_out, self.size as usize, 0))
      .map(|s| s as i32)
  }
}
