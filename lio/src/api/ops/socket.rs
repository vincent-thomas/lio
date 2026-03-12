use std::io;
use std::os::fd::{FromRawFd, RawFd};

use crate::api::resource::Resource;
use crate::typed_op::TypedOp;

// Not detach safe.
pub struct Socket {
  domain: libc::c_int,
  ty: libc::c_int,
  proto: libc::c_int,
}

assert_op_max_size!(Socket);

impl Socket {
  pub(crate) fn new(
    domain: libc::c_int,
    ty: libc::c_int,
    proto: libc::c_int,
  ) -> Self {
    Self { domain, ty, proto }
  }
  //
  // pub fn to_op(self) -> crate::op::Op {
  //   crate::op::Op::Socket {
  //     domain: self.domain,
  //     ty: self.ty,
  //     proto: self.proto,
  //   }
  // }
}

impl TypedOp for Socket {
  type Result = std::io::Result<Resource>;

  fn into_op(&mut self) -> crate::op::Op {
    crate::op::Op::Socket {
      domain: self.domain,
      ty: self.ty,
      proto: self.proto,
    }
  }

  fn extract_result(self, res: isize) -> Self::Result {
    if res < 0 {
      Err(io::Error::from_raw_os_error((-res) as i32))
    } else {
      let fd = res as RawFd;

      // Set SO_REUSEADDR for stream sockets to allow quick rebind after close
      if self.ty == libc::SOCK_STREAM {
        let optval: libc::c_int = 1;
        // SAFETY: fd is valid, optval is a valid pointer to c_int
        unsafe {
          libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_REUSEADDR,
            &optval as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
          );
          // Also set SO_REUSEPORT on platforms that support it (BSD/macOS)
          #[cfg(any(
            target_os = "macos",
            target_os = "ios",
            target_os = "freebsd",
            target_os = "dragonfly",
            target_os = "openbsd",
            target_os = "netbsd"
          ))]
          libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_REUSEPORT,
            &optval as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
          );
        }
      }

      // SAFETY: 'res' is valid fd.
      let resource = unsafe { Resource::from_raw_fd(fd) };
      Ok(resource)
    }
  }

  // #[cfg(unix)]
  // fn meta(&self) -> crate::operation::OpMeta {
  //   crate::operation::OpMeta::CAP_FD | crate::operation::OpMeta::FD_READ
  // }

  // #[cfg(unix)]
  // fn cap(&self) -> i32 {
  //   -1
  // }

  // fn run_blocking(&self) -> isize {
  //   unsafe { libc::socket(self.domain, self.ty, self.proto) as isize }
  // }
}
