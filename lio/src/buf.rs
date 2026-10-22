//! Buffer abstractions for zero-copy I/O operations.
//!
//! This module provides buffer types and traits for efficient I/O without unnecessary
//! allocations. The core abstractions are [`IoBuf`] for write operations (immutable
//! access to data) and [`IoBufMut`] for read operations (mutable access + capacity).
//!
//! # Traits
//!
//! - [`IoBuf`]: For write operations - provides immutable access to data to be written.
//! - [`IoBufMut`]: For read operations - provides mutable access and capacity for reads.
//!
//! The traits expose memory as slices rather than raw pointers, so implementing
//! them requires no `unsafe` and an implementation cannot hand the kernel memory
//! it does not own. Only [`IoBufMut::set_len`] is `unsafe`, and only to *call*:
//! lio calls it after the kernel has initialized the bytes.
//!
//! # Buffers
//!
//! Writes take any [`IoBuf`]: `Vec<u8>`, `Box<[u8]>`, `String`, `Box<str>`,
//! `&'static [u8]`, `&'static str`, `Arc<[u8]>`, `Arc<str>`, `[u8; N]`, and
//! `Cow<'static, [u8]>`. Reads take any [`IoBufMut`]: `Vec<u8>`. With the
//! `bytes` feature, `bytes::Bytes` is an [`IoBuf`] and `bytes::BytesMut` an
//! [`IoBufMut`].
//!
//! [`IoBuf::slice`] and [`IoBufMut::slice_mut`] narrow a buffer to a range
//! without copying it: to send the rest of a buffer after a short write, or
//! to read into the spare capacity after the bytes already there.
//!
//! # Constants
//!
//! - [`MAX_IOV_COUNT`]: Maximum number of buffers for vectored I/O (16).

use std::borrow::Cow;
use std::mem::MaybeUninit;
use std::ops::{Bound, RangeBounds};
use std::sync::Arc;

/// Maximum number of buffers supported for vectored I/O operations.
///
/// This matches the typical kernel limit and ensures efficient syscall handling.
pub const MAX_IOV_COUNT: usize = 16;

/// Result type for operations that return both a result and a buffer.
///
/// This is commonly used for read/write operations where the buffer
/// is returned along with the operation result. The buffer is always
/// returned regardless of success or failure.
///
/// # Example
///
/// ```
/// use lio::BufResult;
///
/// // Simulating an I/O result that returns the buffer
/// let buf = vec![0u8; 1024];
/// let result: BufResult<usize, Vec<u8>> = (Ok(512), buf);
///
/// let (io_result, returned_buf) = result;
/// assert_eq!(io_result.unwrap(), 512);
/// assert_eq!(returned_buf.len(), 1024);
/// ```
pub type BufResult<T, B> = (std::io::Result<T>, B);

/// A buffer for write operations - provides immutable access to data.
///
/// This trait is used for operations that send data (write, send, etc.).
///
/// The slice returned by [`as_slice`](IoBuf::as_slice) must stay at the same
/// address for as long as the buffer is owned by an in-flight operation. lio
/// never moves a buffer while the kernel may be reading it, and only calls
/// `&self` methods on it in that time, so any buffer whose bytes do not change
/// address under `&self` (heap storage, or inline storage of the buffer
/// itself) satisfies this.
pub trait IoBuf: Send + Sync + 'static {
  /// Returns the bytes to write.
  fn as_slice(&self) -> &[u8];

  /// Narrows the buffer to `range` of its bytes, without copying.
  ///
  /// The result writes only those bytes; [`Slice::into_inner`] returns the
  /// whole buffer.
  ///
  /// ```
  /// use lio::IoBuf;
  ///
  /// // After a short write of 2 bytes, send the rest.
  /// let rest = b"hello".to_vec().slice(2..);
  /// assert_eq!(rest.as_slice(), b"llo");
  /// assert_eq!(rest.into_inner(), b"hello");
  /// ```
  ///
  /// # Panics
  ///
  /// If `range` is not within [`as_slice`](IoBuf::as_slice).
  fn slice(self, range: impl RangeBounds<usize>) -> Slice<Self>
  where
    Self: Sized,
  {
    let len = self.as_slice().len();
    let (begin, end) = bounds(range, len);
    Slice { buf: self, begin, end }
  }
}

/// A mutable buffer for read operations - provides mutable access and capacity.
///
/// This trait extends [`IoBuf`] for operations that receive data (read, recv, etc.).
/// It provides mutable access to the buffer's whole capacity and the ability to
/// set the length after a read completes.
pub trait IoBufMut: IoBuf {
  /// Returns the entire writable region of the buffer, from its start up to
  /// its capacity. Reads fill this region from the beginning.
  ///
  /// The first [`as_slice().len()`](IoBuf::as_slice) elements are initialized;
  /// the rest may not be.
  fn as_uninit_mut(&mut self) -> &mut [MaybeUninit<u8>];

  /// Sets the length of valid data in the buffer.
  ///
  /// Called after a read operation completes to indicate how many bytes were read.
  ///
  /// # Safety
  ///
  /// `len` must be at most `as_uninit_mut().len()`, and the first `len` bytes of
  /// [`as_uninit_mut`](IoBufMut::as_uninit_mut) must be initialized.
  unsafe fn set_len(&mut self, len: usize);

  /// Narrows the buffer to `range` of its capacity, without copying: a read
  /// into the result fills that range.
  ///
  /// The range must start within the bytes already initialized. A read
  /// extends the buffer's initialized bytes to the end of what it filled,
  /// and never shrinks them. [`Slice::into_inner`] returns the whole buffer.
  ///
  /// ```
  /// use lio::IoBufMut;
  ///
  /// let mut buf = Vec::with_capacity(16);
  /// buf.extend_from_slice(b"head");
  /// // Read into the spare capacity, after the 4 bytes already there.
  /// let spare = buf.slice_mut(4..);
  /// assert_eq!((spare.begin(), spare.end()), (4, spare.get_ref().capacity()));
  /// ```
  ///
  /// # Panics
  ///
  /// If `range` is not within [`as_uninit_mut`](IoBufMut::as_uninit_mut),
  /// or starts past [`as_slice`](IoBuf::as_slice).
  fn slice_mut(mut self, range: impl RangeBounds<usize>) -> Slice<Self>
  where
    Self: Sized,
  {
    let capacity = self.as_uninit_mut().len();
    let (begin, end) = bounds(range, capacity);
    let init = self.as_slice().len();
    assert!(
      begin <= init,
      "slice_mut: range starts at {begin}, past the {init} initialized bytes"
    );
    Slice { buf: self, begin, end }
  }
}

/// Resolves `range` against a length, panicking if it is out of bounds.
fn bounds(range: impl RangeBounds<usize>, len: usize) -> (usize, usize) {
  let begin = match range.start_bound() {
    Bound::Included(&n) => n,
    Bound::Excluded(&n) => n.checked_add(1).expect("range start overflows"),
    Bound::Unbounded => 0,
  };
  let end = match range.end_bound() {
    Bound::Included(&n) => n.checked_add(1).expect("range end overflows"),
    Bound::Excluded(&n) => n,
    Bound::Unbounded => len,
  };
  assert!(begin <= end, "slice: range starts at {begin} but ends at {end}");
  assert!(end <= len, "slice: range ends at {end}, past the length {len}");
  (begin, end)
}

/// A buffer narrowed to a range, made by [`IoBuf::slice`] or
/// [`IoBufMut::slice_mut`].
///
/// It owns the buffer, and does the I/O on the range alone.
#[derive(Debug)]
pub struct Slice<B> {
  buf: B,
  begin: usize,
  end: usize,
}

impl<B> Slice<B> {
  /// Where the range starts in the buffer.
  pub fn begin(&self) -> usize {
    self.begin
  }

  /// Where the range ends in the buffer (exclusive).
  pub fn end(&self) -> usize {
    self.end
  }

  /// The whole buffer.
  pub fn get_ref(&self) -> &B {
    &self.buf
  }

  /// Returns the whole buffer.
  pub fn into_inner(self) -> B {
    self.buf
  }
}

impl<B: IoBuf> IoBuf for Slice<B> {
  /// The range's initialized bytes: all of it for a write, and for a read
  /// those filled so far.
  fn as_slice(&self) -> &[u8] {
    let init = self.buf.as_slice();
    let end = self.end.min(init.len());
    &init[self.begin.min(end)..end]
  }
}

impl<B: IoBufMut> IoBufMut for Slice<B> {
  fn as_uninit_mut(&mut self) -> &mut [MaybeUninit<u8>] {
    &mut self.buf.as_uninit_mut()[self.begin..self.end]
  }

  unsafe fn set_len(&mut self, len: usize) {
    let init = self.buf.as_slice().len();
    // `slice_mut` checked this, but only the buffer knows its length now.
    assert!(self.begin <= init, "slice: starts past the initialized bytes");
    let filled = self.begin + len;
    // SAFETY: the bytes before `begin` were initialized (checked above), the
    // caller guarantees the `len` after it are, and `filled <= end`, which is
    // within the capacity. Never shrinking keeps bytes past `filled` that
    // were initialized before.
    unsafe { self.buf.set_len(filled.max(init)) }
  }
}

impl IoBuf for Vec<u8> {
  fn as_slice(&self) -> &[u8] {
    self
  }
}

impl IoBufMut for Vec<u8> {
  fn as_uninit_mut(&mut self) -> &mut [MaybeUninit<u8>] {
    let cap = self.capacity();
    // SAFETY: the allocation holds `cap` bytes, and `MaybeUninit<u8>` permits
    // uninitialized contents. The borrow of `self` prevents reallocation.
    unsafe { std::slice::from_raw_parts_mut(self.as_mut_ptr().cast(), cap) }
  }

  unsafe fn set_len(&mut self, len: usize) {
    // SAFETY: the caller guarantees `len <= capacity` and that the first `len`
    // bytes are initialized.
    unsafe { Vec::set_len(self, len) }
  }
}

impl IoBuf for Box<[u8]> {
  fn as_slice(&self) -> &[u8] {
    self
  }
}

impl IoBuf for Box<str> {
  fn as_slice(&self) -> &[u8] {
    self.as_bytes()
  }
}

impl IoBuf for String {
  fn as_slice(&self) -> &[u8] {
    self.as_bytes()
  }
}

impl IoBuf for &'static [u8] {
  fn as_slice(&self) -> &[u8] {
    self
  }
}

impl IoBuf for &'static str {
  fn as_slice(&self) -> &[u8] {
    self.as_bytes()
  }
}

impl IoBuf for Arc<[u8]> {
  fn as_slice(&self) -> &[u8] {
    self
  }
}

impl IoBuf for Arc<str> {
  fn as_slice(&self) -> &[u8] {
    self.as_bytes()
  }
}

impl IoBuf for Cow<'static, [u8]> {
  fn as_slice(&self) -> &[u8] {
    self
  }
}

/// Inline storage: the array moves with the operation that owns it, which
/// stays in place while the operation is in flight.
impl<const N: usize> IoBuf for [u8; N] {
  fn as_slice(&self) -> &[u8] {
    self
  }
}

#[cfg(feature = "bytes")]
impl IoBuf for bytes::Bytes {
  fn as_slice(&self) -> &[u8] {
    self
  }
}

#[cfg(feature = "bytes")]
impl IoBuf for bytes::BytesMut {
  fn as_slice(&self) -> &[u8] {
    self
  }
}

#[cfg(feature = "bytes")]
impl IoBufMut for bytes::BytesMut {
  fn as_uninit_mut(&mut self) -> &mut [MaybeUninit<u8>] {
    // `BytesMut` hands out its whole capacity only as spare capacity, so
    // take that with the length at 0. Its slice derefs only cover the
    // initialized bytes, too little to extend to the capacity.
    let len = self.len();
    // SAFETY: no bytes are claimed initialized.
    unsafe { self.set_len(0) };
    let whole = self.spare_capacity_mut();
    let (ptr, cap) = (whole.as_mut_ptr(), whole.len());
    // SAFETY: the first `len` bytes were initialized and still are.
    unsafe { self.set_len(len) };
    // SAFETY: `ptr` points to the `cap` bytes of the buffer, which setting
    // the length did not touch; `MaybeUninit<u8>` permits uninitialized
    // contents, and the borrow of `self` prevents reallocation.
    unsafe { std::slice::from_raw_parts_mut(ptr, cap) }
  }

  unsafe fn set_len(&mut self, len: usize) {
    // SAFETY: the caller guarantees `len <= capacity` and that the first `len`
    // bytes are initialized.
    unsafe { bytes::BytesMut::set_len(self, len) }
  }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Vectored I/O Buffer Traits (Scatter/Gather)
// ═══════════════════════════════════════════════════════════════════════════════

/// A collection of buffers for vectored writes (`writev`).
///
/// Implemented for any single [`IoBuf`], and for tuples, arrays, and `Vec<B>`
/// of them. The address stability requirement of [`IoBuf`] applies to every
/// buffer.
pub trait IoBufVec {
  /// Returns the number of buffers in the collection.
  fn buf_count(&self) -> usize;

  /// Returns the bytes of buffer at index `i`.
  fn buf(&self, i: usize) -> &[u8];
}

/// The writable regions of an [`IoBufMutVec`], collected by
/// [`IoBufMutVec::bufs_mut`].
///
/// Every region borrows from the collection for the same lifetime `'a`, so the
/// borrow checker proves they are disjoint and all valid at once.
///
/// Holds at most [`MAX_IOV_COUNT`] regions; further pushes are ignored.
pub struct IoSlicesMut<'a> {
  slices: [&'a mut [MaybeUninit<u8>]; MAX_IOV_COUNT],
  len: usize,
}

impl<'a> IoSlicesMut<'a> {
  /// Creates an empty set of regions.
  pub fn new() -> Self {
    Self { slices: std::array::from_fn(|_| <&mut [_]>::default()), len: 0 }
  }

  /// Appends a region. Ignored once [`MAX_IOV_COUNT`] regions are held.
  pub fn push(&mut self, region: &'a mut [MaybeUninit<u8>]) {
    if let Some(slot) = self.slices.get_mut(self.len) {
      *slot = region;
      self.len += 1;
    }
  }

  /// Returns the number of regions held.
  pub fn len(&self) -> usize {
    self.len
  }

  /// Returns true if no regions are held.
  pub fn is_empty(&self) -> bool {
    self.len == 0
  }
}

impl Default for IoSlicesMut<'_> {
  fn default() -> Self {
    Self::new()
  }
}

impl<'a> IntoIterator for IoSlicesMut<'a> {
  type Item = &'a mut [MaybeUninit<u8>];
  type IntoIter =
    std::iter::Take<std::array::IntoIter<Self::Item, MAX_IOV_COUNT>>;

  fn into_iter(self) -> Self::IntoIter {
    self.slices.into_iter().take(self.len)
  }
}

/// A collection of mutable buffers for vectored reads (`readv`).
///
/// Implemented for any single [`IoBufMut`], and for tuples, arrays, and
/// `Vec<B>` of them.
///
/// All regions are handed out by one [`bufs_mut`](IoBufMutVec::bufs_mut) call,
/// so they are borrowed together and cannot move or free one another. As with
/// [`IoBuf`], they must stay in place while the collection is owned by an
/// in-flight operation; lio does not touch the collection in that time.
///
/// # Example
///
/// A header/body pair implemented without `unsafe`:
///
/// ```
/// use lio::{IoBufMut, IoBufMutVec, IoSlicesMut};
///
/// struct Frame {
///   header: Vec<u8>,
///   body: Vec<u8>,
/// }
///
/// impl IoBufMutVec for Frame {
///   fn bufs_mut<'a>(&'a mut self, out: &mut IoSlicesMut<'a>) {
///     out.push(self.header.as_uninit_mut());
///     out.push(self.body.as_uninit_mut());
///   }
///
///   unsafe fn set_buf_len(&mut self, i: usize, len: usize) {
///     let buf = if i == 0 { &mut self.header } else { &mut self.body };
///     // SAFETY: forwarded from the caller.
///     unsafe { IoBufMut::set_len(buf, len) }
///   }
/// }
///
/// let mut frame =
///   Frame { header: Vec::with_capacity(4), body: Vec::with_capacity(16) };
/// let mut regions = IoSlicesMut::new();
/// frame.bufs_mut(&mut regions);
/// assert_eq!(regions.len(), 2);
/// ```
pub trait IoBufMutVec: Send + Sync + 'static {
  /// Pushes the writable region of each buffer, in order, as
  /// [`IoBufMut::as_uninit_mut`] does for a single buffer.
  fn bufs_mut<'a>(&'a mut self, out: &mut IoSlicesMut<'a>);

  /// Sets the length of buffer at index `i`.
  ///
  /// # Safety
  ///
  /// As [`IoBufMut::set_len`], for the `i`-th region pushed by the most recent
  /// [`bufs_mut`](IoBufMutVec::bufs_mut) call.
  unsafe fn set_buf_len(&mut self, i: usize, len: usize);
}

// ═══════════════════════════════════════════════════════════════════════════════
// Single buffer implements vectored traits (a buffer is a 1-element collection)
// ═══════════════════════════════════════════════════════════════════════════════

impl<B: IoBuf> IoBufVec for B {
  fn buf_count(&self) -> usize {
    1
  }
  fn buf(&self, _i: usize) -> &[u8] {
    self.as_slice()
  }
}

impl<B: IoBufMut> IoBufMutVec for B {
  fn bufs_mut<'a>(&'a mut self, out: &mut IoSlicesMut<'a>) {
    out.push(self.as_uninit_mut());
  }
  unsafe fn set_buf_len(&mut self, _i: usize, len: usize) {
    // SAFETY: forwarded from the caller.
    unsafe { self.set_len(len) }
  }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Tuple implementations for IoBufVec/IoBufMutVec
// ═══════════════════════════════════════════════════════════════════════════════

macro_rules! impl_io_buf_vec_tuple {
  ($count:expr, $($idx:tt: $T:ident),+) => {
    impl<$($T: IoBuf),+> IoBufVec for ($($T,)+) {
      fn buf_count(&self) -> usize { $count }

      fn buf(&self, i: usize) -> &[u8] {
        match i {
          $($idx => self.$idx.as_slice(),)+
          _ => panic!("index out of bounds"),
        }
      }
    }

    impl<$($T: IoBufMut),+> IoBufMutVec for ($($T,)+) {
      fn bufs_mut<'a>(&'a mut self, out: &mut IoSlicesMut<'a>) {
        $(out.push(self.$idx.as_uninit_mut());)+
      }

      unsafe fn set_buf_len(&mut self, i: usize, len: usize) {
        match i {
          // SAFETY: forwarded from the caller.
          $($idx => unsafe { self.$idx.set_len(len) },)+
          _ => panic!("index out of bounds"),
        }
      }
    }
  };
}

impl_io_buf_vec_tuple!(1, 0: B0);
impl_io_buf_vec_tuple!(2, 0: B0, 1: B1);
impl_io_buf_vec_tuple!(3, 0: B0, 1: B1, 2: B2);
impl_io_buf_vec_tuple!(4, 0: B0, 1: B1, 2: B2, 3: B3);
impl_io_buf_vec_tuple!(5, 0: B0, 1: B1, 2: B2, 3: B3, 4: B4);
impl_io_buf_vec_tuple!(6, 0: B0, 1: B1, 2: B2, 3: B3, 4: B4, 5: B5);
impl_io_buf_vec_tuple!(7, 0: B0, 1: B1, 2: B2, 3: B3, 4: B4, 5: B5, 6: B6);
impl_io_buf_vec_tuple!(8, 0: B0, 1: B1, 2: B2, 3: B3, 4: B4, 5: B5, 6: B6, 7: B7);

// ═══════════════════════════════════════════════════════════════════════════════
// Array implementations for IoBufVec/IoBufMutVec (up to 16 elements)
// ═══════════════════════════════════════════════════════════════════════════════

macro_rules! impl_io_buf_vec_array {
  ($($n:expr),+) => {
    $(
      impl<B: IoBuf> IoBufVec for [B; $n] {
        fn buf_count(&self) -> usize { $n }

        fn buf(&self, i: usize) -> &[u8] {
          self[i].as_slice()
        }
      }

      impl<B: IoBufMut> IoBufMutVec for [B; $n] {
        fn bufs_mut<'a>(&'a mut self, out: &mut IoSlicesMut<'a>) {
          for buf in self {
            out.push(buf.as_uninit_mut());
          }
        }

        unsafe fn set_buf_len(&mut self, i: usize, len: usize) {
          // SAFETY: forwarded from the caller.
          unsafe { self[i].set_len(len) }
        }
      }
    )+
  };
}

impl_io_buf_vec_array!(1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16);

// ═══════════════════════════════════════════════════════════════════════════════
// Vec implementations for IoBufVec/IoBufMutVec (dynamic buffer count)
// ═══════════════════════════════════════════════════════════════════════════════

impl<B: IoBuf> IoBufVec for Vec<B> {
  fn buf_count(&self) -> usize {
    self.len()
  }

  fn buf(&self, i: usize) -> &[u8] {
    self[i].as_slice()
  }
}

impl<B: IoBuf> IoBufVec for &'static mut Vec<B> {
  fn buf_count(&self) -> usize {
    self.len()
  }

  fn buf(&self, i: usize) -> &[u8] {
    self[i].as_slice()
  }
}

impl<B: IoBufMut> IoBufMutVec for Vec<B> {
  fn bufs_mut<'a>(&'a mut self, out: &mut IoSlicesMut<'a>) {
    for buf in self {
      out.push(buf.as_uninit_mut());
    }
  }

  unsafe fn set_buf_len(&mut self, i: usize, len: usize) {
    // SAFETY: forwarded from the caller.
    unsafe { self[i].set_len(len) }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Fills the first `bytes.len()` bytes of `buf`'s writable region, as a
  /// read would, and reports them.
  fn read_into<B: IoBufMut>(buf: &mut B, bytes: &[u8]) {
    for (dst, src) in buf.as_uninit_mut().iter_mut().zip(bytes) {
      dst.write(*src);
    }
    // SAFETY: the first `bytes.len()` bytes were just written.
    unsafe { buf.set_len(bytes.len()) };
  }

  #[test]
  fn a_slice_writes_its_range_and_returns_the_whole_buffer() {
    let slice = b"hello".to_vec().slice(1..4);
    assert_eq!(slice.as_slice(), b"ell");
    assert_eq!((slice.begin(), slice.end()), (1, 4));
    assert_eq!(slice.get_ref(), b"hello");
    assert_eq!(slice.into_inner(), b"hello");

    assert_eq!(b"hello".to_vec().slice(..).as_slice(), b"hello");
    assert_eq!(b"hello".to_vec().slice(2..=3).as_slice(), b"ll");
    assert_eq!(b"hello".to_vec().slice(5..).as_slice(), b"");
    let excluded_start = (Bound::Excluded(1), Bound::Unbounded);
    assert_eq!("hello".slice(excluded_start).as_slice(), b"llo");
  }

  #[test]
  #[should_panic(expected = "past the length 5")]
  fn a_slice_past_the_end_panics() {
    let _ = b"hello".to_vec().slice(..6);
  }

  #[test]
  #[should_panic(expected = "starts at 3 but ends at 2")]
  fn a_backwards_slice_panics() {
    #[allow(clippy::reversed_empty_ranges)]
    let _ = b"hello".to_vec().slice(3..2);
  }

  #[test]
  fn a_mutable_slice_reads_into_spare_capacity() {
    let mut buf = Vec::with_capacity(16);
    buf.extend_from_slice(b"head");
    let mut spare = buf.slice_mut(4..);
    assert_eq!(spare.as_uninit_mut().len(), spare.get_ref().capacity() - 4);
    assert_eq!(spare.as_slice(), b"", "nothing read yet");

    read_into(&mut spare, b"tail");
    assert_eq!(spare.as_slice(), b"tail");
    assert_eq!(spare.into_inner(), b"headtail");
  }

  #[test]
  fn a_mutable_slice_never_shrinks_the_buffer() {
    let mut slice = b"0123456789".to_vec().slice_mut(2..5);
    assert_eq!(slice.as_uninit_mut().len(), 3);
    read_into(&mut slice, b"ab");
    assert_eq!(slice.as_slice(), b"ab4");
    assert_eq!(slice.into_inner(), b"01ab456789");
  }

  #[test]
  #[should_panic(expected = "past the 4 initialized bytes")]
  fn a_mutable_slice_past_the_initialized_bytes_panics() {
    let mut buf = Vec::with_capacity(16);
    buf.extend_from_slice(b"head");
    let _ = buf.slice_mut(5..);
  }

  #[test]
  #[should_panic(expected = "past the length")]
  fn a_mutable_slice_past_the_capacity_panics() {
    let buf = Vec::<u8>::with_capacity(4);
    let cap = buf.capacity();
    let _ = buf.slice_mut(..cap + 1);
  }

  #[test]
  fn borrowed_and_shared_buffers_write_their_bytes() {
    fn bytes(buf: impl IoBuf) -> Vec<u8> {
      buf.as_slice().to_vec()
    }
    assert_eq!(bytes(&b"static"[..]), b"static");
    assert_eq!(bytes("str"), b"str");
    assert_eq!(bytes(String::from("string")), b"string");
    assert_eq!(bytes(Box::<str>::from("boxed")), b"boxed");
    assert_eq!(bytes(Box::<[u8]>::from(&b"boxed"[..])), b"boxed");
    assert_eq!(bytes(Arc::<[u8]>::from(&b"shared"[..])), b"shared");
    assert_eq!(bytes(Arc::<str>::from("shared")), b"shared");
    assert_eq!(bytes(Cow::Borrowed(&b"cow"[..])), b"cow");
    assert_eq!(bytes(Cow::<[u8]>::Owned(b"cow".to_vec())), b"cow");
    assert_eq!(bytes(*b"array"), b"array");
  }

  #[cfg(feature = "bytes")]
  #[test]
  fn bytes_buffers_write_and_read() {
    let frozen = bytes::Bytes::from_static(b"frozen");
    assert_eq!(IoBuf::as_slice(&frozen), b"frozen");

    let mut buf = bytes::BytesMut::with_capacity(8);
    buf.extend_from_slice(b"ab");
    assert_eq!(IoBuf::as_slice(&buf), b"ab");
    let mut spare = buf.slice_mut(2..);
    read_into(&mut spare, b"cd");
    assert_eq!(&spare.into_inner()[..], b"abcd");
  }

  /// The backend gets the slice's own bytes, in the original allocation.
  mod in_operations {
    use crate::Lio;
    use crate::api;
    use crate::api::resource::Resource;
    use crate::backend::op::Op;
    use crate::backend::testing::{POISON, ScriptedBackend};

    use super::*;

    fn lio() -> (ScriptedBackend, Lio) {
      let backend = ScriptedBackend::new();
      (backend.clone(), Lio::new_with_backend(backend, 1).unwrap())
    }

    /// The single region an in-flight read or write points at.
    fn region(backend: &ScriptedBackend) -> (*const u8, usize) {
      let op = backend.op(backend.pushed()[0]).expect("op in flight");
      let (Op::Write { iovecs, iov_count: 1, .. }
      | Op::Read { iovecs, iov_count: 1, .. }) = op
      else {
        panic!("not a single-buffer read or write: {op:?}");
      };
      // SAFETY: the op is in flight, so its descriptor array is alive.
      let raw = unsafe { iovecs.as_ptr().read() };
      (raw.ptr.cast_const(), raw.len)
    }

    #[test]
    fn a_write_sends_the_rest_after_a_short_write() {
      let (backend, lio) = lio();
      let buf = b"hello world".to_vec();
      let base = buf.as_ptr();
      let done = std::rc::Rc::new(std::cell::RefCell::new(None));
      let out = done.clone();
      api::write(&Resource::stdout(), buf.slice(6..)).with_lio(&lio).when_done(
        move |(res, slice)| {
          *out.borrow_mut() = Some((res.unwrap(), slice.into_inner()));
        },
      );
      assert_eq!(region(&backend), (base.wrapping_add(6), 5));

      backend.complete(backend.pushed()[0], 5);
      lio.try_run().unwrap();
      let (written, buf) = done.borrow_mut().take().unwrap();
      assert_eq!(written, 5);
      assert_eq!(buf.as_ptr(), base, "the same allocation comes back");
    }

    #[test]
    fn a_read_appends_into_spare_capacity() {
      let (backend, lio) = lio();
      let mut buf = Vec::with_capacity(16);
      buf.extend_from_slice(b"head");
      let (base, cap) = (buf.as_ptr(), buf.capacity());
      let done = std::rc::Rc::new(std::cell::RefCell::new(None));
      let out = done.clone();
      api::read(&Resource::stdin(), buf.slice_mut(4..))
        .with_lio(&lio)
        .when_done(move |(res, slice)| {
          *out.borrow_mut() = Some((res.unwrap(), slice.into_inner()));
        });
      assert_eq!(region(&backend), (base.wrapping_add(4), cap - 4));

      // The scripted backend fills the region with `POISON`.
      backend.complete(backend.pushed()[0], 3);
      lio.try_run().unwrap();
      let (read, buf) = done.borrow_mut().take().unwrap();
      assert_eq!(read, 3);
      assert_eq!(buf, [b'h', b'e', b'a', b'd', POISON, POISON, POISON]);
      assert_eq!(buf.as_ptr(), base);
    }
  }
}
