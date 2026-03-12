use std::io;

#[cfg(unix)]
use std::os::fd::RawFd;
#[cfg(windows)]
use std::os::windows::io::RawHandle;

use crate::typed_op::TypedOp;

pub struct Close {
  #[cfg(unix)]
  fd: RawFd,
  #[cfg(windows)]
  handle: RawHandle,
  #[cfg(windows)]
  is_socket: bool,
}

impl Close {
  #[cfg(unix)]
  pub(crate) fn new(fd: RawFd) -> Self {
    Self { fd }
  }

  #[cfg(windows)]
  pub(crate) fn new(handle: RawHandle, is_socket: bool) -> Self {
    Self { handle, is_socket }
  }
}

assert_op_max_size!(Close);

impl TypedOp for Close {
  type Result = io::Result<()>;

  fn into_op(&mut self) -> crate::op::Op {
    #[cfg(unix)]
    {
      crate::op::Op::Close { fd: self.fd }
    }
    #[cfg(windows)]
    {
      crate::op::Op::Close { handle: self.handle, is_socket: self.is_socket }
    }
  }

  fn extract_result(self, res: isize) -> Self::Result {
    if res < 0 {
      Err(io::Error::from_raw_os_error((-res) as i32))
    } else {
      Ok(())
    }
  }
}
