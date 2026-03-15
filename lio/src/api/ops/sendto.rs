use std::net::SocketAddr;

use crate::{
  BufResult, IoBuf, api::resource::Resource, net_utils::std_socketaddr_into_libc,
  api::op::TypedOp,
};

// Note: Using std::marker::Send/Sync because the module contains `Send` struct
pub struct SendTo<B>
where
  B: std::marker::Send + std::marker::Sync,
{
  res: Resource,
  buf: Option<B>,
  flags: i32,
  addr: libc::sockaddr_storage,
  addrlen: libc::socklen_t,
  /// iovec for io_uring sendmsg (stored here so it persists)
  iovec: libc::iovec,
  /// msghdr for io_uring sendmsg (stored here so it persists)
  msghdr: libc::msghdr,
}

// SAFETY: The iovec/msghdr contain raw pointers that point to data within
// this same struct (addr, buf) or to the buffer owned by this struct.
// The operation is only accessed from the owning thread (thread-per-core model).
unsafe impl<B: std::marker::Send + std::marker::Sync> std::marker::Send for SendTo<B> {}
unsafe impl<B: std::marker::Send + std::marker::Sync> std::marker::Sync for SendTo<B> {}

impl<B> SendTo<B>
where
  B: std::marker::Send + std::marker::Sync,
{
  pub(crate) fn new(res: Resource, buf: B, addr: SocketAddr, flags: Option<i32>) -> Self {
    let storage = std_socketaddr_into_libc(addr);
    let addrlen = if addr.is_ipv4() {
      std::mem::size_of::<libc::sockaddr_in>()
    } else {
      std::mem::size_of::<libc::sockaddr_in6>()
    } as libc::socklen_t;
    // SAFETY: iovec and msghdr are C structs safe to zero-initialize
    Self {
      res,
      buf: Some(buf),
      flags: flags.unwrap_or(0),
      addr: storage,
      addrlen,
      iovec: unsafe { std::mem::zeroed() },
      msghdr: unsafe { std::mem::zeroed() },
    }
  }
}

impl<B> TypedOp for SendTo<B>
where
  B: IoBuf,
{
  type Result = BufResult<i32, B>;

  fn into_op(&mut self) -> crate::backend::op::Op {
    let buf = self.buf.as_ref().expect("buffer not available");
    let ptr = buf.as_ptr() as *mut u8;
    let len = buf.len();

    // Set up iovec pointing to the buffer
    self.iovec.iov_base = ptr as *mut _;
    self.iovec.iov_len = len;

    // Set up msghdr pointing to addr and iovec
    self.msghdr.msg_name = &self.addr as *const _ as *mut _;
    self.msghdr.msg_namelen = self.addrlen;
    self.msghdr.msg_iov = &mut self.iovec as *mut _;
    self.msghdr.msg_iovlen = 1;

    crate::backend::op::Op::SendTo {
      fd: self.res.clone(),
      flags: self.flags,
      buf: crate::backend::op::RawBuf::new(ptr, len),
      addr: &self.addr as *const _,
      addrlen: self.addrlen,
      msghdr: &self.msghdr as *const _,
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
