use std::net::SocketAddr;

use crate::{
  IoBufMut, api::resource::Resource, api::op::TypedOp,
  net_utils::libc_socketaddr_into_std,
};

pub struct RecvFrom<B>
where
  B: Send + Sync,
{
  res: Resource,
  buf: Option<B>,
  flags: i32,
  addr: libc::sockaddr_storage,
  addrlen: libc::socklen_t,
  /// iovec for io_uring recvmsg (stored here so it persists)
  iovec: libc::iovec,
  /// msghdr for io_uring recvmsg (stored here so it persists)
  msghdr: libc::msghdr,
}

// SAFETY: The iovec/msghdr contain raw pointers that point to data within
// this same struct (addr, buf) or to the buffer owned by this struct.
// The operation is only accessed from the owning thread (thread-per-core model).
unsafe impl<B: Send + Sync> Send for RecvFrom<B> {}
unsafe impl<B: Send + Sync> Sync for RecvFrom<B> {}

impl<B> RecvFrom<B>
where
  B: Send + Sync,
{
  pub(crate) fn new(res: Resource, buf: B, flags: Option<i32>) -> Self {
    Self {
      res,
      buf: Some(buf),
      flags: flags.unwrap_or(0),
      // SAFETY: sockaddr_storage, iovec, msghdr are C structs safe to zero-initialize
      addr: unsafe { std::mem::zeroed() },
      addrlen: std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t,
      iovec: unsafe { std::mem::zeroed() },
      msghdr: unsafe { std::mem::zeroed() },
    }
  }
}

/// Result type for recvfrom: (io::Result<bytes_received>, buffer, Option<peer_addr>)
pub type RecvFromResult<B> = (std::io::Result<i32>, B, Option<SocketAddr>);

impl<B> TypedOp for RecvFrom<B>
where
  B: IoBufMut,
{
  type Result = RecvFromResult<B>;

  fn into_op(&mut self) -> crate::backend::op::Op {
    let buf = self.buf.as_mut().expect("buffer not available");
    let ptr = buf.as_mut_ptr();
    let len = buf.capacity();

    // Set up iovec pointing to the buffer
    self.iovec.iov_base = ptr as *mut _;
    self.iovec.iov_len = len;

    // Set up msghdr pointing to addr and iovec
    self.msghdr.msg_name = &mut self.addr as *mut _ as *mut _;
    self.msghdr.msg_namelen = self.addrlen;
    self.msghdr.msg_iov = &mut self.iovec as *mut _;
    self.msghdr.msg_iovlen = 1;

    crate::backend::op::Op::RecvFrom {
      fd: self.res.clone(),
      flags: self.flags,
      buf: crate::backend::op::RawBuf::new(ptr, len),
      addr: &mut self.addr as *mut _,
      addrlen: &mut self.addrlen as *mut _,
      msghdr: &mut self.msghdr as *mut _,
    }
  }

  fn extract_result(mut self, res: isize) -> Self::Result {
    let mut buf = self.buf.take().expect("buffer not available");
    if res < 0 {
      (Err(std::io::Error::from_raw_os_error((-res) as i32)), buf, None)
    } else {
      buf.set_len(res as usize);
      // For recvmsg, the actual address length is in msghdr.msg_namelen
      self.addrlen = self.msghdr.msg_namelen;
      let peer_addr = libc_socketaddr_into_std(&self.addr);
      (Ok(res as i32), buf, peer_addr)
    }
  }
}
