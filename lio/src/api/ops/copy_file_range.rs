//! Copy file range operation - server-side file copy.

use crate::api::op::TypedOp;
use crate::api::resource::Resource;

/// Operation to copy data between files without going through userspace (Linux only).
///
/// This performs a server-side copy when possible (e.g., on NFS or reflink-capable
/// filesystems), avoiding data transfer through the application.
#[cfg(target_os = "linux")]
pub struct CopyFileRange {
  fd_in: Resource,
  off_in: i64,
  fd_out: Resource,
  off_out: i64,
  len: usize,
  flags: u32,
}

#[cfg(target_os = "linux")]
assert_op_max_size!(CopyFileRange);

#[cfg(target_os = "linux")]
impl CopyFileRange {
  pub(crate) fn new(
    fd_in: Resource,
    off_in: i64,
    fd_out: Resource,
    off_out: i64,
    len: usize,
    flags: u32,
  ) -> Self {
    Self { fd_in, off_in, fd_out, off_out, len, flags }
  }
}

#[cfg(target_os = "linux")]
impl TypedOp for CopyFileRange {
  type Result = std::io::Result<i32>;

  fn into_op(&mut self) -> crate::backend::op::Op {
    crate::backend::op::Op::CopyFileRange {
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
