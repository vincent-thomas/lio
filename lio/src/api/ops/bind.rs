use std::{io, net::SocketAddr};

use crate::{
  api::resource::Resource, net_utils::std_socketaddr_into_libc,
  typed_op::TypedOp,
};

pub struct Bind {
  res: Resource,
  addr: libc::sockaddr_storage,
}

impl Bind {
  pub(crate) fn new(res: Resource, addr: SocketAddr) -> Self {
    Self { res, addr: std_socketaddr_into_libc(addr) }
  }
}

assert_op_max_size!(Bind);

impl TypedOp for Bind {
  type Result = io::Result<()>;

  fn into_op(&mut self) -> crate::op::Op {
    let addrlen = if self.addr.ss_family == libc::AF_INET as libc::sa_family_t {
      std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t
    } else if self.addr.ss_family == libc::AF_INET6 as libc::sa_family_t {
      std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t
    } else {
      std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t
    };

    crate::op::Op::Bind {
      fd: self.res.clone(),
      addr: &self.addr as *const _,
      addrlen,
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
  //   crate::operation::OpMeta::CAP_FD
  // }

  // #[cfg(unix)]
  // fn cap(&self) -> i32 {
  //   self.res.as_raw_fd()
  // }

  // #[cfg(linux)]
  // fn create_entry(&self) -> lio_uring::submission::Entry {
  //   lio_uring::operation::Bind::new(
  //     self.res.as_raw_fd(),
  //     (&self.addr as *const libc::sockaddr_storage).cast(),
  //     mem::size_of_val(&self.addr) as libc::socklen_t,
  //   )
  //   .build()
  // }

  // fn run_blocking(&self) -> isize {
  //   let storage = self.addr;
  //   let addrlen = if storage.ss_family == libc::AF_INET as libc::sa_family_t {
  //     mem::size_of::<libc::sockaddr_in>()
  //   } else if storage.ss_family == libc::AF_INET6 as libc::sa_family_t {
  //     mem::size_of::<libc::sockaddr_in6>()
  //   } else {
  //     mem::size_of::<libc::sockaddr_storage>()
  //   };

  //   syscall!(raw bind(
  //     self.res.as_raw_fd(),
  //     (&self.addr as *const libc::sockaddr_storage).cast(),
  //     addrlen as libc::socklen_t,
  //   ))
  // }
}
