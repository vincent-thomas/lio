use crate::{BufResult, IoBufVec, MAX_IOV_COUNT, api::op::TypedOp, api::resource::Resource};

pub struct WriteV<B: Send + Sync> {
  res: Resource,
  bufs: Option<B>,
  iovecs: [libc::iovec; MAX_IOV_COUNT],
  iov_count: usize,
}

unsafe impl<B: Send + Sync> Send for WriteV<B> {}
unsafe impl<B: Send + Sync> Sync for WriteV<B> {}

impl<B: Send + Sync> WriteV<B> {
  pub(crate) fn new(res: Resource, bufs: B) -> Self
  where
    B: IoBufVec,
  {
    let iov_count = bufs.buf_count().min(MAX_IOV_COUNT);
    Self {
      res,
      bufs: Some(bufs),
      iovecs: unsafe { std::mem::zeroed() },
      iov_count,
    }
  }
}

impl<B: IoBufVec> TypedOp for WriteV<B> {
  type Result = BufResult<i32, B>;

  fn into_op(&mut self) -> crate::backend::op::Op {
    let bufs = self.bufs.as_ref().expect("buffers not available");

    for i in 0..self.iov_count {
      let (ptr, len) = bufs.buf(i);
      self.iovecs[i].iov_base = ptr as *mut _;
      self.iovecs[i].iov_len = len;
    }

    crate::backend::op::Op::WriteV {
      fd: self.res.clone(),
      buf: crate::backend::op::RawBuf::empty(),
      iovecs: self.iovecs.as_ptr(),
      iov_count: self.iov_count,
    }
  }

  fn extract_result(self, res: isize) -> Self::Result {
    let bufs = self.bufs.expect("buffers not available");
    if res < 0 {
      (Err(std::io::Error::from_raw_os_error((-res) as i32)), bufs)
    } else {
      (Ok(res as i32), bufs)
    }
  }
}
