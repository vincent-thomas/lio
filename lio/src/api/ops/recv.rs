use crate::{
  BufResult, api::resource::Resource, buf::BufLike, typed_op::TypedOp,
};

pub struct Recv<T>
where
  T: Send + Sync,
{
  res: Resource,
  buf: Option<T>,
  flags: i32,
}

impl<T> Recv<T>
where
  T: Send + Sync,
{
  pub(crate) fn new(res: Resource, buf: T, flags: Option<i32>) -> Self {
    Self { res, buf: Some(buf), flags: flags.unwrap_or(0) }
  }

  pub fn to_op(mut self) -> crate::op::Op
  where
    T: BufLike + 'static,
  {
    let buffer = self.buf.take().expect("buffer already taken");
    crate::op::Op::Recv {
      fd: self.res,
      flags: self.flags,
      buffer: crate::op::OpBuf::new(buffer),
    }
  }
}

impl<T> TypedOp for Recv<T>
where
  T: BufLike + Send + Sync + 'static,
{
  type Result = BufResult<i32, T>;

  fn into_op(&mut self) -> crate::op::Op {
    let slice = self.buf.as_mut().expect("buffer not available").buf();
    let ptr = slice.as_ptr() as *mut u8;
    let len = slice.len();
    crate::op::Op::Recv {
      fd: self.res.clone(),
      flags: self.flags,
      buffer: crate::op::OpBuf::new(crate::op::RawBuf { ptr, len }),
    }
  }

  fn extract_result(self, res: isize) -> Self::Result {
    let buf = self.buf.expect("buffer not available");
    if res < 0 {
      // On error, return buffer unchanged
      (Err(std::io::Error::from_raw_os_error((-res) as i32)), buf)
    } else {
      // On success, truncate buffer to received bytes
      (Ok(res as i32), buf.after(res as usize))
    }
  }

  // #[cfg(unix)]
  // fn meta(&self) -> crate::operation::OpMeta {
  //   crate::operation::OpMeta::CAP_FD | crate::operation::OpMeta::FD_READ
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
  //   lio_uring::operation::Recv::new(
  //     self.res.as_raw_fd(),
  //     ptr.cast_mut(),
  //     len as u32,
  //   )
  //   .flags(self.flags)
  //   .build()
  // }

  // fn run_blocking(&self) -> isize {
  //   use std::os::fd::AsRawFd;
  //   let buf_slice = self.buf.as_ref().unwrap().buf();
  //   let ptr = buf_slice.as_ptr();
  //   let len = buf_slice.len();
  //   syscall!(raw recv(self.res.as_raw_fd(), ptr as *mut _, len, self.flags))
  // }
}
