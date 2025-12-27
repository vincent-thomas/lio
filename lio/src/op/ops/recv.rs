use std::{io, os::fd::RawFd};

#[cfg(linux)]
use io_uring::types::Fd;

use crate::{
  BufResult,
  buf::{Buf, BufLike},
  op::{DetachSafe, OpMeta, OperationExt},
};

use crate::op::Operation;

pub struct Recv<T> {
  fd: RawFd,
  buf: Option<Buf<T>>,
  flags: i32,
}

unsafe impl<T> DetachSafe for Recv<T> where T: BufLike {}

impl<T> Recv<T> {
  pub(crate) fn new(fd: RawFd, buf: Buf<T>, flags: Option<i32>) -> Self {
    Self { fd, buf: Some(buf), flags: flags.unwrap_or(0) }
  }
}

impl<T> OperationExt for Recv<T>
where
  T: BufLike,
{
  type Result = BufResult<i32, T>;
}

impl<T> Operation for Recv<T>
where
  T: BufLike,
{
  impl_result!(|this, res: io::Result<i32>| -> BufResult<i32, T> {
    let buf = this.buf.take().expect("ran Recv::result more than once.");
    let out = buf.after(*res.as_ref().unwrap_or(&0) as usize);
    (res, out)
  });

  // #[cfg(linux)]
  // const OPCODE: u8 = 27;

  #[cfg(linux)]
  fn create_entry(&mut self) -> io_uring::squeue::Entry {
    let (ptr, len) = self.buf.as_ref().unwrap().get();
    io_uring::opcode::Recv::new(Fd(self.fd), ptr.cast_mut(), len as u32)
      .flags(self.flags)
      .build()
  }

  fn meta(&self) -> OpMeta {
    OpMeta::CAP_FD | OpMeta::FD_READ
  }

  #[cfg(unix)]
  fn cap(&self) -> i32 {
    self.fd
  }

  fn run_blocking(&self) -> io::Result<i32> {
    let (ptr, len) = self.buf.as_ref().unwrap().get();
    syscall!(recv(self.fd, ptr as *mut _, len, self.flags)).map(|t| t as i32)
  }
}
