use crate::{
  BufResult,
  buf::{Buf, BufLike},
  op::{DetachSafe, Operation, OperationExt},
};

#[cfg(linux)]
use io_uring::types::Fd;

use std::{
  io::{self},
  os::fd::RawFd,
};

pub struct Write<B> {
  fd: RawFd,
  buf: Option<Buf<B>>,
  offset: i64,
}

unsafe impl<B> DetachSafe for Write<B> where B: BufLike {}

impl<B> Write<B> {
  pub(crate) fn new(fd: RawFd, buf: Buf<B>, offset: i64) -> Write<B> {
    Self { fd, buf: Some(buf), offset }
  }
}

impl<B> OperationExt for Write<B>
where
  B: BufLike,
{
  type Result = BufResult<i32, B>;
}

impl<B> Operation for Write<B>
where
  B: BufLike,
{
  impl_result!(|this, res: io::Result<i32>| -> BufResult<i32, B> {
    let buf = this.buf.take().expect("ran Recv::result more than once.");
    let out = buf.after(*res.as_ref().unwrap_or(&0) as usize);
    (res, out)
  });

  impl_no_readyness!();

  #[cfg(linux)]
  const OPCODE: u8 = 23;

  #[cfg(linux)]
  fn create_entry(&mut self) -> io_uring::squeue::Entry {
    let (ptr, len) = self.buf.as_ref().unwrap().get();
    assert!(len <= u32::MAX as usize);
    io_uring::opcode::Write::new(Fd(self.fd), ptr.cast_mut(), len as u32)
      .offset(self.offset as u64)
      .build()
  }

  fn run_blocking(&self) -> std::io::Result<i32> {
    let (ptr, len) = self.buf.as_ref().unwrap().get();

    // For non-seekable fds (pipes, sockets, char devices) with offset -1,
    // use write() instead of pwrite()
    // Try pwrite first
    let result =
      syscall!(pwrite(self.fd, ptr.cast::<libc::c_void>(), len, self.offset))
        .map(|i| i as i32);

    if self.offset == -1 {
      // If pwrite fails with EINVAL (InvalidInput) or ESPIPE (NotSeekable),
      // fall back to write() for non-seekable fds
      if let Err(ref err) = result {
        // I have no idea but InvalidInput is keeping this from
        // killing the process.
        if err.kind() == io::ErrorKind::InvalidInput
          || err.kind() == io::ErrorKind::NotSeekable
        {
          return syscall!(write(
            self.fd,
            ptr.cast::<libc::c_void>() as *const _,
            len,
          ))
          .map(|out| out as i32);
        }
      }

      return result.map(|out| out as i32);
    } else {
      result
    }
  }
}
