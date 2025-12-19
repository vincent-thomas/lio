use super::Operation;
use crate::{BufResult, op::DetachSafe};

#[cfg(linux)]
use io_uring::types::Fd;

use std::{
  io::{self, stdout},
  os::fd::RawFd,
};

#[derive(Debug)]
pub struct Write {
  fd: RawFd,
  buf: Option<Vec<u8>>,
  offset: i64,
}

unsafe impl DetachSafe for Write {}

impl Write {
  pub(crate) fn new(fd: RawFd, buf: Vec<u8>, offset: i64) -> Write {
    Self { fd, buf: Some(buf), offset }
  }
}

impl Operation for Write {
  type Result = BufResult<i32, Vec<u8>>;

  #[cfg(linux)]
  const OPCODE: u8 = 23;

  #[cfg(linux)]
  fn create_entry(&mut self) -> io_uring::squeue::Entry {
    let buf = self.buf.as_ref().unwrap();
    assert!((buf.len()) <= u32::MAX as usize);
    io_uring::opcode::Write::new(Fd(self.fd), buf.as_ptr(), buf.len() as u32)
      .offset(self.offset as u64)
      .build()
  }

  impl_no_readyness!();

  fn run_blocking(&self) -> std::io::Result<i32> {
    // For non-seekable fds (pipes, sockets, char devices) with offset -1,
    // use write() instead of pwrite()
    // Try pwrite first
    let result = syscall!(pwrite(
      self.fd,
      self.buf.as_ref().unwrap().as_ptr() as *const _,
      self.buf.as_ref().unwrap().len(),
      self.offset
    ))
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
            self.buf.as_ref().unwrap().as_ptr() as *const _,
            self.buf.as_ref().unwrap().len(),
          ))
          .map(|out| out as i32);
        }
      }

      return result.map(|out| out as i32);
    } else {
      result
    }
  }

  fn result(&mut self, _ret: std::io::Result<i32>) -> Self::Result {
    let buf = self.buf.take().expect("ran Recv::result more than once.");

    (_ret, buf)
  }
}
