//! Sendfile operation - zero-copy file to socket transfer.

use crate::api::op::TypedOp;
use crate::api::resource::Resource;

/// Operation to send file data to a socket without copying through userspace.
///
/// This is commonly used for serving static files over network sockets.
#[cfg(unix)]
pub struct SendFile {
  out_fd: Resource,
  in_fd: Resource,
  offset: i64,
  count: usize,
}

#[cfg(unix)]
assert_op_max_size!(SendFile);

#[cfg(unix)]
impl SendFile {
  pub(crate) fn new(
    out_fd: Resource,
    in_fd: Resource,
    offset: Option<i64>,
    count: usize,
  ) -> Self {
    Self {
      out_fd,
      in_fd,
      offset: offset.unwrap_or(0),
      count,
    }
  }
}

#[cfg(unix)]
impl TypedOp for SendFile {
  type Result = std::io::Result<i32>;

  fn into_op(&mut self) -> crate::backend::op::Op {
    crate::backend::op::Op::SendFile {
      out_fd: self.out_fd.clone(),
      in_fd: self.in_fd.clone(),
      offset: self.offset,
      count: self.count,
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
