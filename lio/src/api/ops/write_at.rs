use crate::{
  BufResult, api::resource::Resource, buf::BufLike, typed_op::TypedOp,
};

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
  pub(crate) fn new(res: Resource, buf: B, offset: i64) -> Self {
    Self { res, buf: Some(buf), offset }
  }

  pub fn to_op(mut self) -> crate::op::Op
  where
    B: BufLike + 'static,
  {
    let buffer = self.buf.take().expect("buffer already taken");
    crate::op::Op::WriteAt {
      fd: self.res,
      offset: self.offset,
      buffer: crate::op::OpBuf::new(buffer),
    }
  }
}

impl<B> TypedOp for WriteAt<B>
where
  B: BufLike + Send + Sync + 'static,
{
  type Result = BufResult<i32, B>;

  fn into_op(&mut self) -> crate::op::Op {
    let slice = self.buf.as_ref().expect("buffer not available").buf();
    let ptr = slice.as_ptr() as *mut u8;
    let len = slice.len();
    crate::op::Op::WriteAt {
      fd: self.res.clone(),
      offset: self.offset,
      buffer: crate::op::OpBuf::new(crate::op::RawBuf { ptr, len }),
    }
  }

  fn extract_result(self, res: isize) -> Self::Result {
    let buf = self.buf.expect("buffer not available");
    if res < 0 {
      // On error, return buffer unchanged
      (Err(std::io::Error::from_raw_os_error((-res) as i32)), buf)
    } else {
      // On success, advance buffer past written bytes
      (Ok(res as i32), buf.after(res as usize))
    }
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
  //   lio_uring::operation::Write::new(
  //     self.res.as_raw_fd(),
  //     ptr.cast_mut(),
  //     len as u32,
  //   )
  //   .offset(self.offset as u64)
  //   .build()
  // }

  // fn run_blocking(&self) -> isize {
  //   use std::os::fd::AsRawFd;
  //   let buf_slice = self.buf.as_ref().unwrap().buf();
  //   let ptr = buf_slice.as_ptr();
  //   let len = buf_slice.len();

  //   syscall!(raw pwrite(
  //     self.res.as_raw_fd(),
  //     ptr.cast::<libc::c_void>(),
  //     len,
  //     self.offset
  //   ))
  // }
}
