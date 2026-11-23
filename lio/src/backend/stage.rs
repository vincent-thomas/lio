//! Copies caller bytes into memory a backend owns for one submission.
//!
//! The step bump is not this storage. It is reset on the next push. These
//! allocations live in [`StageKeep`] until that lease is dropped, which is
//! after the backend has finished with them.

use std::ffi::OsString;
use std::ptr::{self, NonNull};

use super::op::{
  ByteSpan, DirEntryRef, MsgBuf, MsgBufMut, MsgRecv, MsgSend, Op, RawBuf,
  RawBufMut,
};

/// Where a staged byte or value is copied back to.
enum CopyOut {
  Bytes {
    dst: *mut u8,
    src: *const u8,
    len: usize,
  },
  /// `OsString` owns a heap buffer. A byte copy would free it twice.
  Os {
    dst: *mut OsString,
    src: *const OsString,
  },
}

/// Owned copies of every caller address in one operation.
///
/// The [`Op`] handed to a backend points into this, never at the caller.
pub(crate) struct CopyBack {
  /// One allocation per staged region. Growing this vec does not move those
  /// allocations, so pointers into them stay valid.
  _regions: Vec<Vec<u8>>,
  _desc_mut: Vec<RawBufMut<'static>>,
  _desc: Vec<RawBuf<'static>>,
  _msg_mut: Vec<MsgBufMut>,
  _msg: Vec<MsgBuf>,
  // A `Box` keeps the `OsString` address stable when this vec grows.
  // `Vec<OsString>` would move the string.
  #[allow(clippy::vec_box)]
  _os: Vec<Box<OsString>>,
  copies: Vec<CopyOut>,
  /// Set once the backend has written the staged memory and is about to
  /// drop it. A drop without that still frees the stage and does not write
  /// the caller's bytes.
  publish: bool,
}

impl CopyBack {
  fn empty() -> Self {
    Self {
      _regions: Vec::new(),
      _desc_mut: Vec::new(),
      _desc: Vec::new(),
      _msg_mut: Vec::new(),
      _msg: Vec::new(),
      _os: Vec::new(),
      copies: Vec::new(),
      publish: false,
    }
  }

  fn publish(&self) {
    if !self.publish {
      return;
    }
    for copy in &self.copies {
      match *copy {
        // SAFETY: `src` points into this value's owned allocations. `dst` is
        // the caller buffer captured while it was valid, and the lease that
        // owns this value is dropped only after this copy.
        CopyOut::Bytes { dst, src, len } => unsafe {
          ptr::copy_nonoverlapping(src, dst, len);
        },
        // SAFETY: same lifetime as the byte copies. The clone does not alias
        // the staged string's buffer.
        CopyOut::Os { dst, src } => unsafe {
          *dst = (*src).clone();
        },
      }
    }
  }
}

/// The lease a backend stores with a staged operation.
///
/// [`publish`](Self::publish) copies inbound bytes back to the caller.
/// Dropping it then drops the [`Keep`](super::Keep) that pins the caller's
/// model. A drop without `publish` frees the stage and leaves the caller
/// unchanged. The kernel does not hold this lease.
/// [`IoBackend::drain`](super::IoBackend::drain) still reports whether
/// outside writers are finished with the backend's own memory.
impl std::fmt::Debug for StageKeep {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str("StageKeep")
  }
}

pub struct StageKeep {
  pub(super) back: CopyBack,
  pub(super) keep: super::Keep,
}

impl StageKeep {
  /// A lease with nothing to copy. Tests that complete an operation they
  /// built themselves use this.
  #[cfg(test)]
  pub(crate) fn empty() -> Self {
    Self { back: CopyBack::empty(), keep: super::Keep::new() }
  }

  /// Copies staged inbound bytes back to the caller.
  ///
  /// Call this after the kernel or the backend has finished writing the
  /// staged memory, and before this lease is dropped.
  pub fn publish(&mut self) {
    self.back.publish = true;
    self.back.publish();
    self.back.publish = false;
  }
}

impl Drop for StageKeep {
  fn drop(&mut self) {
    self.back.publish();
  }
}

// SAFETY: the caller pointers are only used on drop, on the thread that
// drops the lease, and the model they name stays alive until the inner
// [`Keep`](super::Keep) drops. Sending the lease sends that permission.
unsafe impl Send for StageKeep {}

/// Builds an [`Op`] whose every caller address has been copied here.
pub(crate) fn stage(op: &Op) -> (Op, CopyBack) {
  let mut staged = op.clone();
  let mut back = CopyBack::empty();
  match &mut staged {
    Op::Read { iovecs, iov_count, .. } => {
      stage_mut(&mut back, iovecs, *iov_count);
    }
    Op::Write { iovecs, iov_count, .. } => {
      stage_init(&mut back, iovecs, *iov_count);
    }
    Op::Recv { msg, .. } => stage_recv(&mut back, msg),
    Op::Send { msg, .. } => stage_send(&mut back, msg),
    Op::Stat { out, .. } => stage_pod(&mut back, out),
    Op::ReadDir {
      raw_buf,
      raw_cap,
      entries,
      entries_cap,
      opaque,
      opaque_drop,
      out,
      ..
    } => {
      stage_raw(&mut back, raw_buf, *raw_cap);
      stage_slice::<DirEntryRef>(&mut back, entries, *entries_cap);
      stage_pod(&mut back, opaque);
      stage_pod(&mut back, opaque_drop);
      stage_pod(&mut back, out);
    }
    Op::Accept { addr, .. } => stage_pod(&mut back, addr),
    Op::ReadlinkAt { buf, buf_len, .. } => stage_raw(&mut back, buf, *buf_len),
    Op::GetCwd { out } => stage_os(&mut back, out),
    _ => {}
  }
  (staged, back)
}

fn stage_mut(
  back: &mut CopyBack,
  iovecs: &mut NonNull<RawBufMut<'static>>,
  count: usize,
) {
  if count == 0 {
    return;
  }
  // SAFETY: the caller holds `count` live descriptors for this call.
  let src = unsafe { std::slice::from_raw_parts(iovecs.as_ptr(), count) };
  let desc_at = back._desc_mut.len();
  for buf in src {
    let len = buf.bytes_len();
    let mut region = vec![0u8; len];
    let src_ptr = region.as_mut_ptr();
    back._desc_mut.push(raw_buf_mut(src_ptr, len));
    back.copies.push(CopyOut::Bytes {
      dst: buf.bytes_ptr(),
      src: src_ptr,
      len,
    });
    back._regions.push(region);
  }
  if count > 0 {
    *iovecs = NonNull::from(&back._desc_mut[desc_at]).cast();
  }
}

fn stage_init(
  back: &mut CopyBack,
  iovecs: &mut NonNull<RawBuf<'static>>,
  count: usize,
) {
  if count == 0 {
    return;
  }
  // SAFETY: the caller holds `count` live descriptors of initialized bytes.
  let src = unsafe { std::slice::from_raw_parts(iovecs.as_ptr(), count) };
  let desc_at = back._desc.len();
  for buf in src {
    let len = buf.bytes_len();
    let mut region = vec![0u8; len];
    let src_ptr = region.as_mut_ptr();
    copy_bytes(buf.bytes_ptr(), src_ptr, len);
    back._desc.push(raw_buf(src_ptr, len));
    back._regions.push(region);
  }
  *iovecs = NonNull::from(&back._desc[desc_at]).cast();
}

fn stage_recv(back: &mut CopyBack, msg: &mut MsgRecv) {
  let count = msg.buf_count.get();
  // SAFETY: `MsgRecv` holds `count` live descriptors.
  let src = unsafe { std::slice::from_raw_parts(msg.bufs.as_ptr(), count) };
  let desc_at = back._msg_mut.len();
  for buf in src {
    let len = buf.len();
    let mut region = vec![0u8; len];
    let src_ptr = region.as_mut_ptr();
    back._msg_mut.push(msg_buf_mut(src_ptr, len));
    back.copies.push(CopyOut::Bytes { dst: buf.as_ptr(), src: src_ptr, len });
    back._regions.push(region);
  }
  msg.bufs = NonNull::from(&back._msg_mut[desc_at]);
  if let Some(from) = msg.from.as_mut() {
    stage_pod(back, from);
  }
}

fn stage_send(back: &mut CopyBack, msg: &mut MsgSend) {
  let count = msg.buf_count.get();
  // SAFETY: `MsgSend` holds `count` live descriptors of initialized bytes.
  let src = unsafe { std::slice::from_raw_parts(msg.bufs.as_ptr(), count) };
  let desc_at = back._msg.len();
  for buf in src {
    let len = buf.len();
    let mut region = vec![0u8; len];
    let src_ptr = region.as_mut_ptr();
    copy_bytes(buf.as_ptr(), src_ptr, len);
    back._msg.push(msg_buf(src_ptr, len));
    back._regions.push(region);
  }
  msg.bufs = NonNull::from(&back._msg[desc_at]);
}

fn stage_raw(back: &mut CopyBack, buf: &mut NonNull<u8>, len: usize) {
  let mut region = vec![0u8; len];
  let src_ptr = region.as_mut_ptr();
  back.copies.push(CopyOut::Bytes { dst: buf.as_ptr(), src: src_ptr, len });
  back._regions.push(region);
  *buf = nn(src_ptr);
}

fn stage_slice<T: Copy>(
  back: &mut CopyBack,
  buf: &mut NonNull<T>,
  count: usize,
) {
  let len = count.saturating_mul(std::mem::size_of::<T>());
  let mut region = vec![0u8; len];
  let src_ptr = region.as_mut_ptr();
  // The caller's previous entries are not an input. The backend fills them.
  back.copies.push(CopyOut::Bytes {
    dst: buf.as_ptr().cast(),
    src: src_ptr,
    len,
  });
  back._regions.push(region);
  *buf = nn(src_ptr.cast());
}

fn stage_pod<T>(back: &mut CopyBack, buf: &mut NonNull<T>) {
  let len = std::mem::size_of::<T>();
  let mut region = vec![0u8; len];
  let src_ptr = region.as_mut_ptr();
  copy_bytes(buf.as_ptr().cast(), src_ptr, len);
  back.copies.push(CopyOut::Bytes {
    dst: buf.as_ptr().cast(),
    src: src_ptr,
    len,
  });
  back._regions.push(region);
  *buf = nn(src_ptr.cast());
}

fn stage_os(back: &mut CopyBack, buf: &mut NonNull<OsString>) {
  // SAFETY: `buf` is the caller's live `OsString` for this call.
  let staged = Box::new(unsafe { buf.as_ref() }.clone());
  let src = NonNull::from(staged.as_ref());
  back.copies.push(CopyOut::Os { dst: buf.as_ptr(), src: src.as_ptr() });
  *buf = src;
  back._os.push(staged);
}

fn nn<T>(ptr: *mut T) -> NonNull<T> {
  // SAFETY: every pointer passed here comes from a `Vec` this stage owns.
  // That allocation is non-null, including the dangling pointer of an empty
  // vec, and it stays put when the owning `Vec` is moved.
  unsafe { NonNull::new_unchecked(ptr) }
}

fn raw_buf_mut(ptr: *mut u8, len: usize) -> RawBufMut<'static> {
  // SAFETY: `ptr`/`len` describe a region this stage owns for the lease.
  unsafe { RawBufMut::from_raw_parts(ptr, len) }
}

fn raw_buf(ptr: *mut u8, len: usize) -> RawBuf<'static> {
  // SAFETY: `ptr`/`len` describe initialized bytes this stage owns.
  unsafe { RawBuf::from_raw_parts(ptr, len) }
}

fn msg_buf_mut(ptr: *mut u8, len: usize) -> MsgBufMut {
  // SAFETY: `ptr` is the non-null base of a region this stage owns.
  unsafe { MsgBufMut::from_raw_parts(nn(ptr), len) }
}

fn msg_buf(ptr: *mut u8, len: usize) -> MsgBuf {
  // SAFETY: `ptr` is the non-null base of initialized bytes this stage owns.
  unsafe { MsgBuf::from_raw_parts(nn(ptr), len) }
}

fn copy_bytes(from: *const u8, to: *mut u8, len: usize) {
  // SAFETY: `from` is a live caller buffer of `len` initialized bytes, and
  // `to` is a stage region of the same length. They do not overlap.
  unsafe { ptr::copy_nonoverlapping(from, to, len) }
}
