// #[cfg(unix)]
use std::{
  io::{self, Error},
  mem::{self},
  net::SocketAddr,
  os::fd::{FromRawFd, RawFd},
};

use crate::{
  api::resource::Resource, net_utils::libc_socketaddr_into_std, op::Op,
  typed_op::TypedOp,
};

// Not detach safe.
pub struct Accept {
  res: Resource,
  addr: Box<libc::sockaddr_storage>,
  len: Box<libc::socklen_t>,
}

// SAFETY: The UnsafeCells are only written during construction and by the kernel
// via syscall output parameters. The operation is consumed after completion, and
// no concurrent mutation occurs, making it safe to send across threads and share references.
// unsafe impl Send for Accept {}
// unsafe impl Sync for Accept {}

impl Accept {
  pub(crate) fn new(res: Resource) -> Self {
    // SAFETY: libc::sockaddr_storage is a C struct that is safe to zero-initialize.
    // It consists of primitive integer fields where zero is a valid value. The kernel
    // will fill this structure via the accept syscall's output parameter.
    let addr: Box<libc::sockaddr_storage> = Box::new(unsafe { mem::zeroed() });
    let len =
      Box::new(mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t);
    Self { res, addr, len }
  }

  pub fn to_op(self) -> crate::op::Op {
    crate::op::Op::Accept {
      fd: self.res,
      addr: Box::into_raw(self.addr),
      len: Box::into_raw(self.len),
    }
  }
}

impl TypedOp for Accept {
  type Result = io::Result<(Resource, SocketAddr)>;

  fn into_op(&mut self) -> crate::op::Op {
    Op::Accept {
      fd: self.res.clone(),
      // SAFETY: self.addr is a Box, so the pointer remains valid even if
      // `self` (the Accept struct) is later moved to the heap by OpCallback.
      addr: &mut *self.addr as *mut _,
      len: &mut *self.len as *mut _,
    }
  }
  fn extract_result(self, res: isize) -> Self::Result {
    let result = if res < 0 {
      return Err(Error::from_raw_os_error(-res as i32));
    } else {
      res as RawFd
    };

    // SAFETY: result is valid fd.
    let res = unsafe { Resource::from_raw_fd(result) };
    // SAFETY: self.addr was filled by the kernel via the accept syscall.
    let addr = unsafe { libc_socketaddr_into_std(&*self.addr as *const _) }?;
    Ok((res, addr))
  }

  // #[cfg(unix)]
  // fn meta(&self) -> crate::operation::OpMeta {
  //   crate::operation::OpMeta::CAP_FD | crate::operation::OpMeta::FD_READ
  // }

  // #[cfg(unix)]
  // fn cap(&self) -> i32 {
  //   self.res.as_raw_fd()
  // }

  // fn run_blocking(&self) -> isize {
  //   use std::os::fd::AsRawFd;
  //   let res = self.res.as_raw_fd();
  //   let mut addr = MaybeUninit::<libc::sockaddr_storage>::uninit();
  //   let mut len =
  //     std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
  //   unsafe {
  //     libc::accept(res, addr.as_mut_ptr() as *mut libc::sockaddr, &mut len)
  //       as isize
  //   }
  // }
}

// impl OperationExt for Accept {
//   type Result = io::Result<(Resource, SocketAddr)>;
// }
//
// impl Operation for Accept {
//   impl_result!(|this, res: isize| -> io::Result<(Resource, SocketAddr)> {
//   });
//
//   // #[cfg(linux)]
//   // const OPCODE: u8 = 13;
//
//   #[cfg(linux)]
//   fn create_entry(&self) -> lio_uring::submission::Entry {
//     lio_uring::operation::Accept::new(
//       self.res.as_raw_fd(),
//       &self.addr as *const _ as *mut libc::sockaddr,
//       (&self.len as *const libc::socklen_t).cast_mut(),
//     )
//     .build()
//   }
//
//   fn meta(&self) -> OpMeta {
//     OpMeta::CAP_FD | OpMeta::FD_READ
//   }
//
//   #[cfg(unix)]
//   fn cap(&self) -> i32 {
//     self.res.as_raw_fd()
//   }
//
//   fn run_blocking(&self) -> isize {
//     #[cfg(any(
//       target_os = "android",
//       target_os = "dragonfly",
//       target_os = "freebsd",
//       target_os = "illumos",
//       target_os = "linux",
//       target_os = "hurd",
//       target_os = "netbsd",
//       target_os = "openbsd",
//       target_os = "cygwin",
//     ))]
//     let fd = {
//       syscall!(raw accept4(
//         self.res.as_raw_fd(),
//         &self.addr as *const _ as *mut libc::sockaddr,
//         (&self.len as *const libc::socklen_t).cast_mut(),
//         libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK
//       ))
//     };
//
//     #[cfg(not(any(
//       target_os = "android",
//       target_os = "dragonfly",
//       target_os = "freebsd",
//       target_os = "illumos",
//       target_os = "linux",
//       target_os = "hurd",
//       target_os = "netbsd",
//       target_os = "openbsd",
//       target_os = "cygwin",
//     )))]
//     let fd = {
//       let socket = syscall!(raw accept(
//         self.res.as_raw_fd(),
//         &self.addr as *const _ as *mut libc::sockaddr,
//         (&self.len as *const libc::socklen_t).cast_mut()
//       )?);
//
//       if socket < 0 {
//         return socket;
//       }
//
//       // Ensure the socket is closed if either of the `fcntl` calls error below
//       #[cfg(not(any(target_os = "espidf", target_os = "vita")))]
//       {
//         let res =
//           syscall!(raw fcntl(socket as i32, libc::F_SETFD, libc::FD_CLOEXEC));
//         if res < 0 {
//           syscall!(raw close(socket as i32));
//           return res;
//         }
//       }
//
//       // See https://github.com/tokio-rs/mio/issues/1450
//       #[cfg(not(any(target_os = "espidf", target_os = "vita")))]
//       {
//         let res =
//           syscall!(raw fcntl(socket as i32, libc::F_SETFL, libc::O_NONBLOCK));
//         if res < 0 {
//           let _ = syscall!(raw close(socket as i32));
//           return res;
//         }
//       }
//
//       socket
//     };
//
//     fd
//   }
// }
// assert_op_max_size!(Accept);
