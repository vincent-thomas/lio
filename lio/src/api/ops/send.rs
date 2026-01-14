use std::os::fd::AsRawFd;

use crate::{BufResult, api::resource::Resource, buf::BufLike};

use crate::operation::{OpMeta, Operation, OperationExt};

pub struct Send<B>
where
  B: std::marker::Send + std::marker::Sync,
{
  res: Resource,
  buf: Option<B>,
  flags: i32,
}

impl<B> Send<B>
where
  B: std::marker::Send + std::marker::Sync,
{
  pub(crate) fn new(res: Resource, buf: B, flags: Option<i32>) -> Self {
    // assert!((buf.len()) <= u32::MAX as usize);
    Self { res, buf: Some(buf), flags: flags.unwrap_or(0) }
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
  impl_result!(|this, res: isize| -> BufResult<i32, B> {
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
  // const OPCODE: u8 = 26;

  #[cfg(linux)]
  fn create_entry(&self) -> lio_uring::submission::Entry {
    let buf_slice = self.buf.as_ref().unwrap().buf();
    let ptr = buf_slice.as_ptr();
    let len = buf_slice.len();
    lio_uring::operation::Send::new(self.res.as_raw_fd(), ptr, len as u32)
      .flags(self.flags)
      .build()
  }

  fn meta(&self) -> crate::operation::OpMeta {
    OpMeta::CAP_FD | OpMeta::FD_WRITE
  }
  #[cfg(unix)]
  fn cap(&self) -> i32 {
    self.res.as_raw_fd()
  }

  fn run_blocking(&self) -> isize {
    let buf_slice = self.buf.as_ref().unwrap().buf();
    let ptr = buf_slice.as_ptr();
    let len = buf_slice.len();
    syscall!(raw send(self.res.as_raw_fd(), ptr as *mut _, len, self.flags))
  }
}
