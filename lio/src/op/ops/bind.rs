use std::os::fd::AsRawFd;
use std::{cell::UnsafeCell, io, mem, net::SocketAddr};

#[cfg(linux)]
use io_uring::types::Fd;

use crate::op::net_utils::std_socketaddr_into_libc;

use crate::op::{Operation, OperationExt};
use crate::resource::Resource;

pub struct Bind {
  res: Resource,
  addr: UnsafeCell<libc::sockaddr_storage>, // addr: SocketAddr,
}

// SAFETY: The UnsafeCell is only written during construction and read during
// execution. No mutation occurs after the operation is created, making it safe
// to send across threads and share references.
unsafe impl Send for Bind {}
unsafe impl Sync for Bind {}

impl Bind {
  pub(crate) fn new(res: Resource, addr: SocketAddr) -> Self {
    Self { res, addr: UnsafeCell::new(std_socketaddr_into_libc(addr)) }
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
  fn create_entry(&self) -> io_uring::squeue::Entry {
    io_uring::opcode::Bind::new(
      Fd(self.res.as_raw_fd()),
      self.addr.get().cast(),
      mem::size_of_val(&self.addr) as libc::socklen_t,
    )
    .build()
  }

  fn run_blocking(&self) -> isize {
    let storage = unsafe { &*self.addr.get() };
    let addrlen = if storage.ss_family == libc::AF_INET as libc::sa_family_t {
      mem::size_of::<libc::sockaddr_in>()
    } else if storage.ss_family == libc::AF_INET6 as libc::sa_family_t {
      mem::size_of::<libc::sockaddr_in6>()
    } else {
      mem::size_of::<libc::sockaddr_storage>()
    };

    syscall_raw!(bind(
      self.res.as_raw_fd(),
      self.addr.get().cast(),
      addrlen as libc::socklen_t,
    ))
  }
}
