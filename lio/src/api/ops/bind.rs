use std::{io, mem, net::SocketAddr, os::fd::AsRawFd};

use crate::{
  api::resource::Resource,
  net_utils::std_socketaddr_into_libc,
  operation::{Operation, OperationExt},
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

impl OperationExt for Bind {
  type Result = io::Result<()>;
}

impl Operation for Bind {
  impl_no_readyness!();
  impl_result!(());

  // #[cfg(linux)]
  // const OPCODE: u8 = 56;

  #[cfg(linux)]
  fn create_entry(&self) -> lio_uring::submission::Entry {
    lio_uring::operation::Bind::new(
      self.res.as_raw_fd(),
      (&self.addr as *const libc::sockaddr_storage).cast(),
      mem::size_of_val(&self.addr) as libc::socklen_t,
    )
    .build()
  }

  fn run_blocking(&self) -> isize {
    let storage = self.addr;
    let addrlen = if storage.ss_family == libc::AF_INET as libc::sa_family_t {
      mem::size_of::<libc::sockaddr_in>()
    } else if storage.ss_family == libc::AF_INET6 as libc::sa_family_t {
      mem::size_of::<libc::sockaddr_in6>()
    } else {
      mem::size_of::<libc::sockaddr_storage>()
    };

    syscall!(raw bind(
      self.res.as_raw_fd(),
      (&self.addr as *const libc::sockaddr_storage).cast(),
      addrlen as libc::socklen_t,
    ))
  }
}
