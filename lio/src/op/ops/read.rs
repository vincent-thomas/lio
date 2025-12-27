use std::{io, os::fd::RawFd};

#[cfg(linux)]
use io_uring::types::Fd;

use crate::{
  BufResult,
  buf::{Buf, BufLike},
  op::DetachSafe,
};

use crate::op::{Operation, OperationExt};

pub struct Read<T> {
  fd: RawFd,
  buf: Option<Buf<T>>,
  offset: i64,
}

unsafe impl<T> DetachSafe for Read<T> where T: BufLike {}

impl<T> Read<T> {
  /// Will return errn 22 "EINVAL" if offset < 0
  pub(crate) fn new(fd: RawFd, mem: Buf<T>, offset: i64) -> Self
  where
    T: BufLike,
  {
    Self { fd, buf: Some(mem), offset }
  }
}

impl<T> OperationExt for Read<T>
where
  T: BufLike,
{
  type Result = BufResult<i32, T>;
}

impl<T> Operation for Read<T>
where
  T: BufLike,
{
  impl_result!(|this, ret: io::Result<i32>| -> BufResult<i32, T> {
    let buf = this.buf.take().expect("ran Recv::result more than once.");
    let out = buf.after(*ret.as_ref().unwrap_or(&0) as usize);
    (ret, out)
  });

  impl_no_readyness!();

  #[cfg(linux)]
  fn create_entry(&mut self) -> io_uring::squeue::Entry {
    let (ptr, len) = self.buf.as_ref().unwrap().get();
    io_uring::opcode::Read::new(Fd(self.fd), ptr.cast_mut(), len as u32)
      .offset(self.offset as u64)
      .build()
  }

  fn run_blocking(&self) -> io::Result<i32> {
    let (ptr, len) = self.buf.as_ref().unwrap().get();
    syscall!(pread(self.fd, ptr as *mut _, len, self.offset)).map(|t| t as i32)
  }
}
