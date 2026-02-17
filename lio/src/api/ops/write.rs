use std::os::fd::AsRawFd;

use crate::{
  BufResult, api::resource::Resource, buf::BufLike, typed_op::TypedOp,
};

pub struct Write<B>
where
  B: Send + Sync,
{
  res: Resource,
  buf: Option<B>,
}

impl<B> Write<B>
where
  B: Send + Sync,
{
  pub(crate) fn new(res: Resource, buf: B) -> Self {
    Self { res, buf: Some(buf) }
  }

  pub fn to_op(mut self) -> crate::op::Op
  where
    B: BufLike + 'static,
  {
    let buffer = self.buf.take().expect("buffer already taken");
    crate::op::Op::Write {
      fd: self.res,
      buffer: crate::op::ErasedBuffer::new(buffer),
    }
  }
}

impl<B> TypedOp for Write<B>
where
  B: BufLike + Send + Sync + 'static,
{
  type Result = BufResult<i32, B>;

  fn into_op(&mut self) -> crate::op::Op {
    let buffer = self.buf.take().expect("buffer already taken");
    crate::op::Op::Write {
      fd: self.res.clone(),
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
  //   lio_uring::operation::Write::new(
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
  //   let fd = self.res.as_raw_fd();
  //   syscall!(raw write(fd, ptr.cast::<libc::c_void>() as *const _, len))
  // }
}
