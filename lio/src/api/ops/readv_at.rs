use crate::{BufResult, IoBufMutVec, MAX_IOV_COUNT, api::op::TypedOp, api::resource::Resource};

pub struct ReadVAt<B: Send + Sync> {
  res: Resource,
  bufs: Option<B>,
  iovecs: [libc::iovec; MAX_IOV_COUNT],
  iov_count: usize,
  offset: i64,
}

unsafe impl<B: Send + Sync> Send for ReadVAt<B> {}
unsafe impl<B: Send + Sync> Sync for ReadVAt<B> {}

impl<B: Send + Sync> ReadVAt<B> {
  pub(crate) fn new(res: Resource, bufs: B, offset: i64) -> Self
  where
    B: IoBufMutVec,
  {
    let iov_count = bufs.buf_count().min(MAX_IOV_COUNT);
    Self {
      res,
      bufs: Some(bufs),
      iovecs: unsafe { std::mem::zeroed() },
      iov_count,
      offset,
    }
  }
}

impl<B: IoBufMutVec> TypedOp for ReadVAt<B> {
  type Result = BufResult<i32, B>;

  fn into_op(&mut self) -> crate::backend::op::Op {
    let bufs = self.bufs.as_mut().expect("buffers not available");

    for i in 0..self.iov_count {
      let (ptr, cap) = bufs.buf_mut(i);
      self.iovecs[i].iov_base = ptr as *mut _;
      self.iovecs[i].iov_len = cap;
    }

    crate::backend::op::Op::ReadVAt {
      fd: self.res.clone(),
      buf: crate::backend::op::RawBuf::empty(),
      iovecs: self.iovecs.as_ptr(),
      iov_count: self.iov_count,
      offset: self.offset,
    }
  }

  fn extract_result(mut self, res: isize) -> Self::Result {
    let mut bufs = self.bufs.take().expect("buffers not available");
    if res < 0 {
      (Err(std::io::Error::from_raw_os_error((-res) as i32)), bufs)
    } else {
      // Distribute total bytes read across buffers using stored capacities
      let mut remaining = res as usize;
      for i in 0..self.iov_count {
        let cap = self.iovecs[i].iov_len;
        let len = remaining.min(cap);
        bufs.set_buf_len(i, len);
        remaining = remaining.saturating_sub(cap);
      }
      (Ok(res as i32), bufs)
    }
  }
}
