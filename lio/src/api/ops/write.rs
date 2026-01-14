use crate::{
  BufResult,
  api::resource::Resource,
  buf::BufLike,
  operation::{Operation, OperationExt},
};

use std::os::fd::AsRawFd;

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
  pub(crate) fn new(res: Resource, buf: B) -> Write<B> {
    Self { res, buf: Some(buf) }
  }
}

impl<B> OperationExt for Write<B>
where
  B: BufLike + Send + Sync,
{
  type Result = BufResult<i32, B>;
}

impl<B> Operation for Write<B>
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
  fn create_entry(&self) -> lio_uring::submission::Entry {
    let buf_slice = self.buf.as_ref().unwrap().buf();
    let ptr = buf_slice.as_ptr();
    let len = buf_slice.len();
    assert!(len <= u32::MAX as usize);
    lio_uring::operation::Write::new(
      self.res.as_raw_fd(),
      ptr.cast_mut(),
      len as u32,
    )
    .offset(-1i64 as u64)
    .build()
  }

  fn run_blocking(&self) -> isize {
    let buf_slice = self.buf.as_ref().unwrap().buf();
    let ptr = buf_slice.as_ptr();
    let len = buf_slice.len();
    let fd = self.res.as_raw_fd();

    syscall!(raw write(fd, ptr.cast::<libc::c_void>() as *const _, len))
  }
}
