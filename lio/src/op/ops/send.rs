use std::{io, os::fd::RawFd};

#[cfg(linux)]
use io_uring::types::Fd;

use crate::{
  BufResult,
  buf::{Buf, BufLike},
  op::{DetachSafe, OpMeta},
};

use crate::op::{Operation, OperationExt};

pub struct Send<B> {
  fd: RawFd,
  buf: Option<Buf<B>>,
  flags: i32,
}

unsafe impl<B> DetachSafe for Send<B> where B: BufLike {}

impl<B> Send<B> {
  pub(crate) fn new(fd: RawFd, buf: Buf<B>, flags: Option<i32>) -> Self {
    // assert!((buf.len()) <= u32::MAX as usize);
    Self { fd, buf: Some(buf), flags: flags.unwrap_or(0) }
  }
}

impl<B> OperationExt for Send<B>
where
  B: BufLike,
{
  type Result = BufResult<i32, B>;
}

impl<B> Operation for Send<B>
where
  B: BufLike,
{
  impl_result!(|this, res: std::io::Result<i32>| -> BufResult<i32, B> {
    let buf = this.buf.take().expect("ran Recv::result more than once.");
    let out = buf.after(*res.as_ref().unwrap_or(&0) as usize);
    (res, out)
  });

  // #[cfg(linux)]
  // const OPCODE: u8 = 26;

  #[cfg(linux)]
  fn create_entry(&mut self) -> io_uring::squeue::Entry {
    let (ptr, len) = self.buf.as_ref().unwrap().get();
    io_uring::opcode::Send::new(Fd(self.fd), ptr, len as u32)
      .flags(self.flags)
      .build()
  }

  fn meta(&self) -> crate::op::OpMeta {
    OpMeta::CAP_FD | OpMeta::FD_WRITE
  }
  #[cfg(unix)]
  fn cap(&self) -> i32 {
    self.fd
  }

  fn run_blocking(&self) -> io::Result<i32> {
    let (ptr, len) = self.buf.as_ref().unwrap().get();
    syscall!(send(self.fd, ptr as *mut _, len, self.flags)).map(|t| t as i32)
  }
}
