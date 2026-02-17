use std::os::fd::AsRawFd;

use crate::{
  BufResult, api::resource::Resource, buf::BufLike, typed_op::TypedOp,
};

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
    Self { res, buf: Some(buf), flags: flags.unwrap_or(0) }
  }

  pub fn to_op(mut self) -> crate::op::Op
  where
    B: BufLike + 'static,
  {
    let buffer = self.buf.take().expect("buffer already taken");
    crate::op::Op::Send {
      fd: self.res,
      flags: self.flags,
      buffer: crate::op::ErasedBuffer::new(buffer),
    }
  }
}

impl<B> TypedOp for Send<B>
where
  B: BufLike + std::marker::Send + std::marker::Sync + 'static,
{
  type Result = BufResult<i32, B>;

  fn into_op(&mut self) -> crate::op::Op {
    let buffer = self.buf.take().expect("buffer already taken");
    crate::op::Op::Send {
      fd: self.res.clone(),
      flags: self.flags,
      buffer: crate::op::ErasedBuffer::new(buffer),
    }
  }

  fn extract_result(self, res: isize) -> Self::Result {
    let buf = self.buf.expect("buffer already taken");
    let bytes = if res < 0 { 0 } else { res as usize };
    let out = buf.after(bytes);
    let result = if res < 0 {
      Err(std::io::Error::from_raw_os_error((-res) as i32))
    } else {
      Ok(res as i32)
    };
    (result, out)
  }

  // #[cfg(unix)]
  // fn meta(&self) -> crate::operation::OpMeta {
  //   crate::operation::OpMeta::CAP_FD | crate::operation::OpMeta::FD_WRITE
  // }

  // #[cfg(unix)]
  // fn cap(&self) -> i32 {
  //   self.res.as_raw_fd()
  // }

  // #[cfg(linux)]
  // fn create_entry(&self) -> lio_uring::submission::Entry {
  //   use std::os::fd::AsRawFd;
  //   let buf_slice = self.buf.as_ref().unwrap().buf();
  //   let ptr = buf_slice.as_ptr();
  //   let len = buf_slice.len();
  //   lio_uring::operation::Send::new(self.res.as_raw_fd(), ptr, len as u32)
  //     .flags(self.flags)
  //     .build()
  // }

  // fn run_blocking(&self) -> isize {
  //   use std::os::fd::AsRawFd;
  //   let buf_slice = self.buf.as_ref().unwrap().buf();
  //   let ptr = buf_slice.as_ptr();
  //   let len = buf_slice.len();
  //   syscall!(raw send(self.res.as_raw_fd(), ptr as *mut _, len, self.flags))
  // }
}
