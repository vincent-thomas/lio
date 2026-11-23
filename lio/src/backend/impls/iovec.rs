//! Copy portable buffer descriptors into one native `iovec` array.
//!
//! io_uring and the poller both lower reads, writes, sends, and receives
//! through this. The array is fixed-size. A longer list is rejected so the
//! kernel cannot read past it.

use std::ptr::NonNull;

use crate::backend::op::ByteSpan;
use crate::buf::MAX_IOV_COUNT;

/// Copy `count` spans starting at `first` into a native iovec array.
///
/// Returns `None` when `count` is greater than [`MAX_IOV_COUNT`]. The
/// pointer is not read in that case. Unused slots stay null and zero-length.
///
/// # Safety
///
/// When `count` is at most [`MAX_IOV_COUNT`], `first` must point at `count`
/// live spans. Each span's address stays valid for as long as the returned
/// iovecs are used.
pub(crate) unsafe fn fill_iovecs<B: ByteSpan>(
  first: NonNull<B>,
  count: usize,
) -> Option<[libc::iovec; MAX_IOV_COUNT]> {
  if count > MAX_IOV_COUNT {
    return None;
  }
  // SAFETY: `count` is in range, and the caller guarantees that many live spans.
  let spans = unsafe { std::slice::from_raw_parts(first.as_ptr(), count) };
  let mut native =
    [libc::iovec { iov_base: std::ptr::null_mut(), iov_len: 0 }; MAX_IOV_COUNT];
  for (dst, src) in native.iter_mut().zip(spans.iter()) {
    dst.iov_base = src.bytes_ptr().cast();
    dst.iov_len = src.bytes_len();
  }
  Some(native)
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::backend::op::{MsgBuf, MsgBufMut, RawBuf, RawBufMut};

  #[test]
  fn copies_each_span_and_leaves_the_rest_empty() {
    let bytes = [9u8, 8, 7];
    let raw = [RawBuf::new(&bytes)];
    // SAFETY: `raw` is one live span, and `count` is 1.
    let native = unsafe { fill_iovecs(NonNull::from(&raw[0]), 1) }.unwrap();
    assert_eq!(native[0].iov_base.cast::<u8>(), bytes.as_ptr().cast_mut());
    assert_eq!(native[0].iov_len, bytes.len());
    assert!(native[1].iov_base.is_null());
    assert_eq!(native[1].iov_len, 0);

    let msg = [MsgBuf::from_slice(&bytes)];
    // SAFETY: `msg` is one live span, and `count` is 1.
    let native = unsafe { fill_iovecs(NonNull::from(&msg[0]), 1) }.unwrap();
    assert_eq!(native[0].iov_base.cast::<u8>(), bytes.as_ptr().cast_mut());
    assert_eq!(native[0].iov_len, bytes.len());

    let mut owned = *b"ab";
    let msg_mut = [MsgBufMut::from_slice(&mut owned)];
    // SAFETY: `msg_mut` is one live span, and `count` is 1.
    let native = unsafe { fill_iovecs(NonNull::from(&msg_mut[0]), 1) }.unwrap();
    assert_eq!(native[0].iov_base.cast::<u8>(), owned.as_mut_ptr());
    assert_eq!(native[0].iov_len, owned.len());

    let mut raw_bytes = [1u8, 2];
    let raw_mut = [RawBufMut::from_mut(&mut raw_bytes)];
    // SAFETY: `raw_mut` is one live span, and `count` is 1.
    let native = unsafe { fill_iovecs(NonNull::from(&raw_mut[0]), 1) }.unwrap();
    assert_eq!(native[0].iov_base.cast::<u8>(), raw_bytes.as_mut_ptr());
    assert_eq!(native[0].iov_len, raw_bytes.len());
  }

  #[test]
  fn rejects_a_list_past_the_native_array() {
    let raw = RawBuf::empty();
    // SAFETY: a count past the array is rejected before the pointer is read.
    let got = unsafe { fill_iovecs(NonNull::from(&raw), MAX_IOV_COUNT + 1) };
    assert!(got.is_none());
  }

  #[test]
  fn accepts_an_empty_list_without_reading_spans() {
    let raw = RawBuf::empty();
    // SAFETY: `count` is 0, so no span is read.
    let native = unsafe { fill_iovecs(NonNull::from(&raw), 0) }.unwrap();
    assert!(
      native.iter().all(|iov| iov.iov_base.is_null() && iov.iov_len == 0)
    );
  }
}
