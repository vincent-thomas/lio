//! Splice operation - zero-copy data transfer between file descriptors via pipe.

use crate::api::op::TypedOp;
use crate::api::resource::Resource;

/// Operation to splice data between file descriptors (Linux only).
///
/// At least one of `fd_in` or `fd_out` must be a pipe. This enables zero-copy
/// data transfer between a pipe and another file descriptor.
#[cfg(target_os = "linux")]
pub struct Splice {
  fd_in: Resource,
  off_in: i64,
  fd_out: Resource,
  off_out: i64,
  len: u32,
  flags: u32,
}

#[cfg(target_os = "linux")]
assert_op_max_size!(Splice);

#[cfg(target_os = "linux")]
impl Splice {
  pub(crate) fn new(
    fd_in: Resource,
    off_in: Option<i64>,
    fd_out: Resource,
    off_out: Option<i64>,
    len: u32,
    flags: u32,
  ) -> Self {
    Self {
      fd_in,
      off_in: off_in.unwrap_or(-1),
      fd_out,
      off_out: off_out.unwrap_or(-1),
      len,
      flags,
    }
  }
}

#[cfg(target_os = "linux")]
impl TypedOp for Splice {
  type Result = std::io::Result<i32>;

  fn into_op(&mut self) -> crate::backend::op::Op {
    crate::backend::op::Op::Splice {
      fd_in: self.fd_in.clone(),
      off_in: self.off_in,
      fd_out: self.fd_out.clone(),
      off_out: self.off_out,
      len: self.len,
      flags: self.flags,
    }
  }

  fn extract_result(self, res: isize) -> Self::Result {
    if res < 0 {
      Err(std::io::Error::from_raw_os_error((-res) as i32))
    } else {
      Ok(res as i32)
    }
  }
}
