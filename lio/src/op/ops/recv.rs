use std::os::fd::{AsFd, AsRawFd};

#[cfg(linux)]
use io_uring::types::Fd;

use crate::{
  BufResult,
  buf::BufLike,
  op::{DetachSafe, OpMeta, OperationExt},
  resource::Resource,
};

use crate::op::Operation;

pub struct Recv<T>
where
  T: Send + Sync,
{
  res: Resource,
  buf: Option<T>,
  flags: i32,
}

unsafe impl<T> DetachSafe for Recv<T> where T: BufLike + Send + Sync {}

impl<T> Recv<T>
where
  T: Send + Sync,
{
  pub(crate) fn new(res: Resource, buf: T, flags: Option<i32>) -> Self {
    Self { res, buf: Some(buf), flags: flags.unwrap_or(0) }
  }
}

impl<T> OperationExt for Recv<T>
where
  T: BufLike + Send + Sync,
{
  type Result = BufResult<i32, T>;
}

impl<T> Operation for Recv<T>
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

  // #[cfg(linux)]
  // const OPCODE: u8 = 27;

  #[cfg(linux)]
  fn create_entry(&self) -> io_uring::squeue::Entry {
    let buf_slice = self.buf.as_ref().unwrap().buf();
    let ptr = buf_slice.as_ptr();
    let len = buf_slice.len();
    io_uring::opcode::Recv::new(Fd(self.res.as_fd().as_raw_fd()), ptr.cast_mut(), len as u32)
      .flags(self.flags)
      .build()
  }

  fn meta(&self) -> OpMeta {
    OpMeta::CAP_FD | OpMeta::FD_READ
  }

  #[cfg(unix)]
  fn cap(&self) -> i32 {
    self.res.as_fd().as_raw_fd()
  }

  fn run_blocking(&self) -> isize {
    let buf_slice = self.buf.as_ref().unwrap().buf();
    let ptr = buf_slice.as_ptr();
    let len = buf_slice.len();
    syscall_raw!(recv(self.res.as_fd().as_raw_fd(), ptr as *mut _, len, self.flags))
  }
}
