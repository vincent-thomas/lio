use std::cell::UnsafeCell;
use std::net::SocketAddr;
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::{io, mem};

#[cfg(linux)]
use io_uring::types::Fd;

use crate::op::net_utils::std_socketaddr_into_libc;
use crate::op::{DetachSafe, OpMeta, OperationExt};
use crate::resource::Resource;

use crate::op::Operation;

pub struct Connect {
  res: Resource,
  addr: UnsafeCell<libc::sockaddr_storage>,
  connect_called: AtomicBool,
}

assert_op_max_size!(Connect);

// SAFETY: The UnsafeCell is only written during construction and read during
// execution. No mutation occurs after the operation is created, making it safe
// to send across threads and share references.
unsafe impl Send for Connect {}
unsafe impl Sync for Connect {}

unsafe impl DetachSafe for Connect {}

impl Connect {
  pub(crate) fn new(res: Resource, addr: SocketAddr) -> Self {
    Self {
      res,
      addr: UnsafeCell::new(std_socketaddr_into_libc(addr)),
      connect_called: AtomicBool::new(false),
    }
  }

  fn get_addrlen(&self) -> libc::socklen_t {
    let storage = unsafe { &*self.addr.get() };
    let addrlen = if storage.ss_family == libc::AF_INET as libc::sa_family_t {
      mem::size_of::<libc::sockaddr_in>()
    } else if storage.ss_family == libc::AF_INET6 as libc::sa_family_t {
      mem::size_of::<libc::sockaddr_in6>()
    } else {
      mem::size_of::<libc::sockaddr_storage>()
    };

    addrlen as libc::socklen_t
  }
}

impl OperationExt for Connect {
  type Result = io::Result<()>;
}

impl Operation for Connect {
  impl_result!(());

  // #[cfg(linux)]
  // const OPCODE: u8 = 16;

  #[cfg(linux)]
  fn create_entry(&self) -> io_uring::squeue::Entry {
    io_uring::opcode::Connect::new(
      Fd(self.res.as_raw_fd()),
      self.addr.get().cast(),
      self.get_addrlen(),
    )
    .build()
  }

  fn meta(&self) -> OpMeta {
    OpMeta::CAP_FD | OpMeta::FD_WRITE
  }

  fn cap(&self) -> i32 {
    self.res.as_raw_fd()
  }

  fn run_blocking(&self) -> isize {
    let result = unsafe {
      libc::connect(
        self.res.as_raw_fd(),
        self.addr.get().cast(),
        self.get_addrlen(),
      )
    } as isize;

    // Track if this is the first connect() call for this operation
    let is_first_call = !self.connect_called.swap(true, Ordering::SeqCst);

    // - If this is a subsequent call: connection just completed (success)
    // - If this is the first connect() call: socket was already connected (error)
    // if result < 0 {
    if result == -libc::EISCONN as isize && is_first_call {
      //   // First connect() returned EISCONN = socket was already connected
      return -libc::EISCONN as isize;
    }

    result
  }
}
