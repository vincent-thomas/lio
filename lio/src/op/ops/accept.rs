#[cfg(unix)]
use std::os::fd::{AsFd, AsRawFd, FromRawFd, RawFd};
use std::{
  cell::UnsafeCell,
  io::{self, Error},
  mem::{self},
  net::SocketAddr,
};

#[cfg(linux)]
use io_uring::{opcode, squeue, types::Fd};

use crate::{
  op::{OpMeta, OperationExt, net_utils::libc_socketaddr_into_std},
  resource::Resource,
};

use crate::op::Operation;

// Not detach safe.
pub struct Accept {
  res: Resource,
  addr: UnsafeCell<libc::sockaddr_storage>,
  len: UnsafeCell<libc::socklen_t>,
}

// SAFETY: The UnsafeCells are only written during construction and by the kernel
// via syscall output parameters. The operation is consumed after completion, and
// no concurrent mutation occurs, making it safe to send across threads and share references.
unsafe impl Send for Accept {}
unsafe impl Sync for Accept {}

impl Accept {
  pub(crate) fn new(res: impl Into<Resource>) -> Self {
    let addr: libc::sockaddr_storage = unsafe { mem::zeroed() };
    Self {
      res: res.into(),
      addr: UnsafeCell::new(addr),
      len: UnsafeCell::new(mem::size_of_val(&addr) as libc::socklen_t),
    }
  }
}

impl OperationExt for Accept {
  type Result = io::Result<(Resource, SocketAddr)>;
}

impl Operation for Accept {
  impl_result!(|this, res: isize| -> io::Result<(Resource, SocketAddr)> {
    let result = if res < 0 {
      return Err(Error::from_raw_os_error(-res as i32));
    } else {
      res as RawFd
    };

    let res = unsafe { Resource::from_raw_fd(result) };

    Ok((res, libc_socketaddr_into_std(this.addr.get())?))
  });

  // #[cfg(linux)]
  // const OPCODE: u8 = 13;

  #[cfg(linux)]
  fn create_entry(&self) -> squeue::Entry {
    opcode::Accept::new(
      Fd(self.res.as_fd().as_raw_fd()),
      self.addr.get().cast::<libc::sockaddr>(),
      self.len.get(),
    )
    .build()
  }

  fn meta(&self) -> OpMeta {
    OpMeta::CAP_FD | OpMeta::FD_READ
  }

  #[cfg(unix)]
  fn cap(&self) -> i32 {
    self.res.as_fd().as_raw_fd()
  }

  fn run_blocking(&self) -> isize {
    #[cfg(any(
      target_os = "android",
      target_os = "dragonfly",
      target_os = "freebsd",
      target_os = "illumos",
      target_os = "linux",
      target_os = "hurd",
      target_os = "netbsd",
      target_os = "openbsd",
      target_os = "cygwin",
    ))]
    let fd = {
      syscall_raw!(accept4(
        self.res.as_fd().as_raw_fd(),
        self.addr.get() as *mut libc::sockaddr,
        self.len.get() as *mut libc::socklen_t,
        libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK
      ))
    };

    #[cfg(not(any(
      target_os = "android",
      target_os = "dragonfly",
      target_os = "freebsd",
      target_os = "illumos",
      target_os = "linux",
      target_os = "hurd",
      target_os = "netbsd",
      target_os = "openbsd",
      target_os = "cygwin",
    )))]
    let fd = {
      let socket = syscall_raw!(accept(
        self.res.as_fd().as_raw_fd(),
        self.addr.get() as *mut libc::sockaddr,
        self.len.get() as *mut libc::socklen_t
      ));

      if socket < 0 {
        return socket;
      }

      // Ensure the socket is closed if either of the `fcntl` calls error below
      #[cfg(not(any(target_os = "espidf", target_os = "vita")))]
      {
        let res =
          syscall_raw!(fcntl(socket as i32, libc::F_SETFD, libc::FD_CLOEXEC));
        if res < 0 {
          unsafe { libc::close(socket as i32) };
          return res;
        }
      }

      // See https://github.com/tokio-rs/mio/issues/1450
      #[cfg(not(any(target_os = "espidf", target_os = "vita")))]
      {
        let res =
          syscall_raw!(fcntl(socket as i32, libc::F_SETFL, libc::O_NONBLOCK));
        if res < 0 {
          unsafe { libc::close(socket as i32) };
          return res;
        }
      }

      socket
    };

    fd
  }
}
assert_op_max_size!(Accept);
