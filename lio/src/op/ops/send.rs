use std::{io, os::fd::RawFd};

#[cfg(linux)]
use io_uring::types::Fd;

use crate::{
  BufResult,
  buf::BufLike,
  op::{DetachSafe, OpMeta},
};

use crate::op::{Operation, OperationExt};

pub struct Send<B>
where
  B: std::marker::Send + std::marker::Sync,
{
  fd: RawFd,
  buf: Option<B>,
  flags: i32,
}

unsafe impl<B> DetachSafe for Send<B> where B: BufLike + std::marker::Send + std::marker::Sync {}

impl<B> Send<B>
where
  B: std::marker::Send + std::marker::Sync,
{
  pub(crate) fn new(fd: RawFd, buf: B, flags: Option<i32>) -> Self {
    // assert!((buf.len()) <= u32::MAX as usize);
    Self { fd, buf: Some(buf), flags: flags.unwrap_or(0) }
  }
}

impl<B> OperationExt for Send<B>
where
  B: BufLike + std::marker::Send + std::marker::Sync,
{
  type Result = BufResult<i32, B>;
}

impl<B> Operation for Send<B>
where
  B: BufLike + std::marker::Send + std::marker::Sync,
{
  impl_result!(|this, res: std::io::Result<i32>| -> BufResult<i32, B> {
    let buf = this.buf.take().expect("ran Recv::result more than once.");
    let out = buf.after(*res.as_ref().unwrap_or(&0) as usize);
    (res, out)
  });

  // #[cfg(linux)]
  // const OPCODE: u8 = 26;

  #[cfg(linux)]
  fn create_entry(&self) -> io_uring::squeue::Entry {
    let buf_slice = self.buf.as_ref().unwrap().buf();
    let ptr = buf_slice.as_ptr();
    let len = buf_slice.len();
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
    let buf_slice = self.buf.as_ref().unwrap().buf();
    let ptr = buf_slice.as_ptr();
    let len = buf_slice.len();
    syscall!(send(self.fd, ptr as *mut _, len, self.flags)).map(|t| t as i32)
  }
}
