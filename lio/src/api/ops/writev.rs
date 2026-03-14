use crate::{BufResult, api::resource::Resource, typed_op::TypedOp};

/// Gather-write operation: writes from multiple buffers to a file descriptor.
///
/// Wraps the `writev(2)` syscall. The kernel writes the contents of all provided
/// buffers in order to the file descriptor in a single call.
pub struct Writev {
  res: Resource,
  bufs: Option<Vec<Vec<u8>>>,
  /// Iovec array built from `bufs` by `into_op`. Kept alive here so the
  /// pointer stored in the `OpBuf` remains valid while the op is in flight.
  iovecs: Vec<libc::iovec>,
}

assert_op_max_size!(Writev);

// SAFETY: `iovecs` contains raw pointers that point into `bufs`. Both `bufs`
// and `iovecs` are owned by this (boxed) TypedOp and are only accessed from
// the thread that scheduled the operation.
unsafe impl Send for Writev {}
// SAFETY: Same reasoning as Send.
unsafe impl Sync for Writev {}

impl Writev {
  pub(crate) fn new(res: Resource, bufs: Vec<Vec<u8>>) -> Self {
    let iovecs = Vec::with_capacity(bufs.len());
    Self { res, bufs: Some(bufs), iovecs }
  }
}

impl TypedOp for Writev {
  type Result = BufResult<i32, Vec<Vec<u8>>>;

  fn into_op(&mut self) -> crate::op::Op {
    let bufs = self.bufs.as_mut().expect("buffers not available");
    // Rebuild the iovec array so every pointer is current.
    self.iovecs.clear();
    for buf in bufs.iter_mut() {
      self.iovecs.push(libc::iovec {
        // Cast away const: libc::iovec.iov_base is *mut c_void but for writes
        // the kernel only reads from it.
        iov_base: buf.as_ptr() as *mut libc::c_void,
        iov_len: buf.len(),
      });
    }
    let ptr = self.iovecs.as_ptr();
    let len = self.iovecs.len() as u32;
    crate::op::Op::Writev {
      fd: self.res.clone(),
      // SAFETY: `ptr` points into `self.iovecs` which is owned by this (boxed)
      // TypedOp and will not move or be freed until `extract_result` is called.
      buffer: crate::op::OpBuf::new(crate::op::RawIovecBuf { ptr, len }),
    }
  }

  fn extract_result(self, res: isize) -> Self::Result {
    let bufs = self.bufs.expect("buffers not available");
    if res < 0 {
      (Err(std::io::Error::from_raw_os_error((-res) as i32)), bufs)
    } else {
      (Ok(res as i32), bufs)
    }
  }
}
