//! Accept operation for Unix domain sockets.
//!
//! Unlike the regular `Accept` operation which returns a `SocketAddr`,
//! this operation returns only the accepted file descriptor since Unix
//! domain socket addresses don't map to `std::net::SocketAddr`.

use std::{
  io::{self, Error},
  mem,
  os::fd::{FromRawFd, RawFd},
};

use crate::{api::resource::Resource, op::Op, typed_op::TypedOp};

/// Accept operation for Unix domain sockets.
///
/// This is a variant of `Accept` specifically for Unix domain sockets.
/// It returns only the accepted `Resource` without attempting to parse
/// the peer address into a `SocketAddr`.
pub struct AcceptUnix {
  res: Resource,
  addr: Box<libc::sockaddr_storage>,
  len: Box<libc::socklen_t>,
}

impl AcceptUnix {
  pub(crate) fn new(res: Resource) -> Self {
    // SAFETY: libc::sockaddr_storage is a C struct that is safe to zero-initialize.
    let addr: Box<libc::sockaddr_storage> = Box::new(unsafe { mem::zeroed() });
    let len =
      Box::new(mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t);
    Self { res, addr, len }
  }
}

impl TypedOp for AcceptUnix {
  type Result = io::Result<Resource>;

  fn into_op(&mut self) -> Op {
    Op::Accept {
      fd: self.res.clone(),
      addr: &mut *self.addr as *mut _,
      len: &mut *self.len as *mut _,
    }
  }

  fn extract_result(self, res: isize) -> Self::Result {
    if res < 0 {
      return Err(Error::from_raw_os_error(-res as i32));
    }

    // SAFETY: result is valid fd returned by accept syscall.
    let resource = unsafe { Resource::from_raw_fd(res as RawFd) };
    Ok(resource)
  }
}
