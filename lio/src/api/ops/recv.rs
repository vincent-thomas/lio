use crate::{BufResult, IoBufMut, api::resource::Resource, api::op::TypedOp};

pub struct Recv<B>
where
  B: Send + Sync,
{
  res: Resource,
  buf: Option<B>,
  flags: i32,
}

impl<B> Recv<B>
where
  B: Send + Sync,
{
  pub(crate) fn new(res: Resource, buf: B, flags: Option<i32>) -> Self {
    Self { res, buf: Some(buf), flags: flags.unwrap_or(0) }
  }
}

impl<B> TypedOp for Recv<B>
where
  B: IoBufMut,
{
  type Result = BufResult<i32, B>;

  fn into_op(&mut self) -> crate::backend::op::Op {
    let buf = self.buf.as_mut().expect("buffer not available");
    let ptr = buf.as_mut_ptr();
    let len = buf.capacity();
    crate::backend::op::Op::Recv {
      fd: self.res.clone(),
      flags: self.flags,
      buf: crate::backend::op::RawBuf::new(ptr, len),
    }
  }

  fn extract_result(mut self, res: isize) -> Self::Result {
    let mut buf = self.buf.take().expect("buffer not available");
    if res < 0 {
      (Err(std::io::Error::from_raw_os_error((-res) as i32)), buf)
    } else {
      buf.set_len(res as usize);
      (Ok(res as i32), buf)
    }
  }
}
