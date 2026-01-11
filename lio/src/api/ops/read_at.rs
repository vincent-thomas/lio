use std::os::fd::AsRawFd;

#[cfg(linux)]
use io_uring::types::Fd;

use crate::{BufResult, buf::BufLike, api::resource::Resource};

use crate::operation::{Operation, OperationExt};

pub struct ReadAt<T>
where
  T: Send + Sync,
{
  res: Resource,
  buf: Option<T>,
  offset: i64,
}

impl<T> ReadAt<T>
where
  T: Send + Sync,
{
  /// Will return errn 22 "EINVAL" if offset < 0
  pub(crate) fn new(res: Resource, mem: T, offset: i64) -> Self
  where
    T: BufLike,
  {
    Self { res, buf: Some(mem), offset }
  }
}

impl<T> OperationExt for ReadAt<T>
where
  T: BufLike + Send + Sync,
{
  type Result = BufResult<i32, T>;
}

impl<T> Operation for ReadAt<T>
where
  T: BufLike + Send + Sync,
{
  impl_result!(|this, res: isize| -> BufResult<i32, T> {
    let buf = this.buf.take().expect("ran Recv::result more than once.");
    let bytes = if res < 0 { 0 } else { res as usize };
    let out = buf.after(bytes);
    let result = if res < 0 {
      Err(std::io::Error::from_raw_os_error((-res) as i32))
    } else {
      Ok(res as i32)
    };
    (result, out)
  });

  impl_no_readyness!();

  #[cfg(linux)]
  fn create_entry(&self) -> io_uring::squeue::Entry {
    let buf_slice = self.buf.as_ref().unwrap().buf();
    let ptr = buf_slice.as_ptr();
    let len = buf_slice.len();
    io_uring::opcode::Read::new(
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
    syscall!(raw pread(self.res.as_raw_fd(), ptr as *mut _, len, self.offset))
  }
}
