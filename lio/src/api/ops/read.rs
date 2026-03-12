use crate::typed_op::TypedOp;
use crate::{BufResult, api::resource::Resource, buf::BufLike};

pub struct Read<T>
where
  T: Send + Sync,
{
  res: Resource,
  buf: Option<T>,
}

impl<T> Read<T>
where
  T: Send + Sync,
{
  /// Will return errn 22 "EINVAL" if offset < 0
  pub(crate) fn new(res: Resource, mem: T) -> Self
  where
    T: BufLike,
  {
    Self { res, buf: Some(mem) }
  }

  pub fn to_op(mut self) -> crate::op::Op
  where
    T: BufLike + 'static,
  {
    let buffer = self.buf.take().expect("buffer already taken");
    crate::op::Op::Read { fd: self.res, buffer: crate::op::OpBuf::new(buffer) }
  }
}

impl<T> TypedOp for Read<T>
where
  T: BufLike + Send + Sync + 'static,
{
  type Result = BufResult<i32, T>;

  fn into_op(&mut self) -> crate::op::Op {
    let slice = self.buf.as_mut().expect("buffer not available").buf();
    let ptr = slice.as_ptr() as *mut u8;
    let len = slice.len();
    crate::op::Op::Read {
      fd: self.res.clone(),
      buffer: crate::op::OpBuf::new(crate::op::RawBuf { ptr, len }),
    }
  }

  fn extract_result(self, res: isize) -> Self::Result {
    let buf = self.buf.expect("buffer not available");
    if res < 0 {
      // On error, return buffer unchanged
      (Err(std::io::Error::from_raw_os_error((-res) as i32)), buf)
    } else {
      // On success, truncate buffer to read bytes
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
  //   lio_uring::operation::Read::new(
  //     self.res.as_raw_fd(),
  //     ptr.cast_mut(),
  //     len as u32,
  //   )
  //   .offset(-1i64 as u64)
  //   .build()
  // }

  // fn run_blocking(&self) -> isize {
  //   use std::os::fd::AsRawFd;
  //   let buf_slice = self.buf.as_ref().unwrap().buf();
  //   let ptr = buf_slice.as_ptr();
  //   let len = buf_slice.len();
  //   unsafe { libc::read(self.res.as_raw_fd(), ptr as *mut _, len) }
  // }
}
