use std::{
  io,
  net::SocketAddr,
  os::fd::AsRawFd,
  sync::atomic::{AtomicBool, Ordering},
};

use crate::{
  api::resource::Resource, net_utils::std_socketaddr_into_libc,
  typed_op::TypedOp,
};

pub struct Connect {
  res: Resource,
  addr: libc::sockaddr_storage,
  len: libc::socklen_t,
  connect_called: AtomicBool,
}

assert_op_max_size!(Connect);

impl Connect {
  pub(crate) fn new(res: Resource, addr: SocketAddr) -> Self {
    let addr = std_socketaddr_into_libc(addr);
    let len = if addr.ss_family == libc::AF_INET as libc::sa_family_t {
      std::mem::size_of::<libc::sockaddr_in>()
    } else if addr.ss_family == libc::AF_INET6 as libc::sa_family_t {
      std::mem::size_of::<libc::sockaddr_in6>()
    } else {
      std::mem::size_of::<libc::sockaddr_storage>()
    } as libc::socklen_t;
    Self { res, addr, len, connect_called: AtomicBool::new(false) }
  }

  /// Convert this operation into an Op enum variant.
  pub fn to_op(self) -> crate::op::Op {
    let connect_called = self.connect_called.load(Ordering::Relaxed);
    crate::op::Op::Connect {
      fd: self.res,
      addr: self.addr,
      len: self.len,
      connect_called,
    }
  }
}

impl TypedOp for Connect {
  type Result = io::Result<()>;

  fn into_op(&mut self) -> crate::op::Op {
    let connect_called = self.connect_called.load(Ordering::Relaxed);
    crate::op::Op::Connect {
      fd: self.res.clone(),
      addr: self.addr,
      len: self.len,
      connect_called,
    }
  }

  fn extract_result(self, res: isize) -> Self::Result {
    if res < 0 {
      Err(io::Error::from_raw_os_error((-res) as i32))
    } else {
      Ok(())
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

  // fn run_blocking(&self) -> isize {
  //   use std::os::fd::AsRawFd;
  //   let ret = unsafe {
  //     libc::connect(
  //       self.res.as_raw_fd(),
  //       &self.addr as *const _ as *const _,
  //       self.len,
  //     )
  //   };
  //   if ret < 0 {
  //     let err = unsafe { *libc::__error() };
  //     if err == libc::EINPROGRESS || err == libc::EISCONN {
  //       return if err == libc::EISCONN { 0 } else { -err as isize };
  //     }
  //   }
  //   ret as isize
  // }
}
