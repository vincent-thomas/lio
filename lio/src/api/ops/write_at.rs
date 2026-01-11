use crate::{
  BufResult,
  api::resource::Resource,
  buf::BufLike,
  operation::{Operation, OperationExt},
};

#[cfg(linux)]
use io_uring::types::Fd;

use std::os::fd::AsRawFd;

pub struct WriteAt<B>
where
  B: Send + Sync,
{
  res: Resource,
  buf: Option<B>,
  offset: i64,
}

impl<B> WriteAt<B>
where
  B: Send + Sync,
{
  pub(crate) fn new(res: Resource, buf: B, offset: i64) -> WriteAt<B> {
    Self { res, buf: Some(buf), offset }
  }
}

impl<B> OperationExt for WriteAt<B>
where
  B: BufLike + Send + Sync,
{
  type Result = BufResult<i32, B>;
}

impl<B> Operation for WriteAt<B>
where
  B: BufLike + Send + Sync,
{
  impl_result!(|this, res: isize| -> BufResult<i32, B> {
    let buf = this.buf.take().expect("ran Write::result more than once.");
    let bytes_written = if res < 0 { 0 } else { res as usize };
    let out = buf.after(bytes_written);
    let result = if res < 0 {
      Err(std::io::Error::from_raw_os_error((-res) as i32))
    } else {
      Ok(res as i32)
    };
    (result, out)
  });

  impl_no_readyness!();

  #[cfg(linux)]
  // const OPCODE: u8 = 23;
  #[cfg(linux)]
  fn create_entry(&self) -> io_uring::squeue::Entry {
    let buf_slice = self.buf.as_ref().unwrap().buf();
    let ptr = buf_slice.as_ptr();
    let len = buf_slice.len();
    assert!(len <= u32::MAX as usize);
    io_uring::opcode::Write::new(
      Fd(self.res.as_raw_fd()),
      ptr.cast_mut(),
      len as u32,
    )
    .offset(self.offset as u64)
    .build()
  }

  fn run_blocking(&self) -> isize {
    let buf_slice = self.buf.as_ref().unwrap().buf();
    let ptr = buf_slice.as_ptr();
    let len = buf_slice.len();

    // For non-seekable fds (pipes, sockets, char devices) with offset -1,
    // use write() instead of pwrite()
    syscall!(raw pwrite(
      self.res.as_raw_fd(),
      ptr.cast::<libc::c_void>(),
      len,
      self.offset
    ))
  }
}
