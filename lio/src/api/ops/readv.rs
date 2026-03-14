use crate::{BufResult, api::resource::Resource, typed_op::TypedOp};

/// Scatter-read operation: reads from a file descriptor into multiple buffers.
///
/// Wraps the `readv(2)` syscall. The kernel fills the provided buffers in order,
/// reading up to the combined capacity of all buffers in a single call.
pub struct Readv {
  res: Resource,
  bufs: Option<Vec<Vec<u8>>>,
  /// Iovec array built from `bufs` by `into_op`. Kept alive here so the
  /// pointer stored in the `OpBuf` remains valid while the op is in flight.
  iovecs: Vec<libc::iovec>,
}

assert_op_max_size!(Readv);

// SAFETY: `iovecs` contains raw pointers that point into `bufs`. Both `bufs`
// and `iovecs` are owned by this (boxed) TypedOp and are only accessed from
// the thread that scheduled the operation.
unsafe impl Send for Readv {}
// SAFETY: Same reasoning as Send.
unsafe impl Sync for Readv {}

impl Readv {
  pub(crate) fn new(res: Resource, bufs: Vec<Vec<u8>>) -> Self {
    let iovecs = Vec::with_capacity(bufs.len());
    Self { res, bufs: Some(bufs), iovecs }
  }
}

impl TypedOp for Readv {
  type Result = BufResult<i32, Vec<Vec<u8>>>;

  fn into_op(&mut self) -> crate::op::Op {
    let bufs = self.bufs.as_mut().expect("buffers not available");
    // Rebuild the iovec array so every pointer is current (handles the case
    // where the Vec backing stores may have moved since last into_op call).
    self.iovecs.clear();
    for buf in bufs.iter_mut() {
      self.iovecs.push(libc::iovec {
        iov_base: buf.as_mut_ptr() as *mut libc::c_void,
        // Use capacity so the kernel can fill up to the pre-allocated size.
        iov_len: buf.capacity(),
      });
    }
    let ptr = self.iovecs.as_ptr();
    let len = self.iovecs.len() as u32;
    crate::op::Op::Readv {
      fd: self.res.clone(),
      // SAFETY: `ptr` points into `self.iovecs` which is owned by this (boxed)
      // TypedOp and will not move or be freed until `extract_result` is called.
      buffer: crate::op::OpBuf::new(crate::op::RawIovecBuf { ptr, len }),
    }
  }

  fn extract_result(self, res: isize) -> Self::Result {
    let mut bufs = self.bufs.expect("buffers not available");
    if res < 0 {
      (Err(std::io::Error::from_raw_os_error((-res) as i32)), bufs)
    } else {
      // Distribute the bytes read across the buffers in order.
      let mut remaining = res as usize;
      for buf in &mut bufs {
        let cap = buf.capacity();
        if remaining >= cap {
          // SAFETY: The kernel filled `cap` bytes into this buffer.
          unsafe { buf.set_len(cap) };
          remaining -= cap;
        } else {
          // SAFETY: The kernel filled `remaining` bytes into this buffer.
          unsafe { buf.set_len(remaining) };
          break;
        }
      }
      (Ok(res as i32), bufs)
    }
  }
}
