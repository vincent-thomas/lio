use crate::{BufResult, IoBuf, api::resource::Resource, api::op::TypedOp};

// Note: Using std::marker::Send/Sync because the struct is named `Send`
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
}

impl<B> TypedOp for Send<B>
where
  B: IoBuf,
{
  type Result = BufResult<i32, B>;

  fn into_op(&mut self) -> crate::backend::op::Op {
    let buf = self.buf.as_ref().expect("buffer not available");
    let ptr = buf.as_ptr() as *mut u8;
    let len = buf.len();
    crate::backend::op::Op::Send {
      fd: self.res.clone(),
      flags: self.flags,
      buf: crate::backend::op::RawBuf::new(ptr, len),
    }
  }

  fn extract_result(self, res: isize) -> Self::Result {
    let buf = self.buf.expect("buffer not available");
    if res < 0 {
      (Err(std::io::Error::from_raw_os_error((-res) as i32)), buf)
    } else {
      (Ok(res as i32), buf)
    }
  }
}
