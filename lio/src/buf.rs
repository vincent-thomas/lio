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
//! # Constants
//!
//! - [`MAX_IOV_COUNT`]: Maximum number of buffers for vectored I/O (16).
//!
//! # Feature `zeroize`
//!
//! When the `zeroize` feature is enabled, buffers can be
//! automatically and securely zeroed out on drop. This provides
//! defense-in-depth against memory disclosure vulnerabilities by ensuring sensitive
//! data doesn't persist in pooled buffers.
//!
//! This uses the [`zeroize`](https://docs.rs/zeroize) crate, which guarantees that
//! compiler optimizations won't eliminate the zeroing operation.

use std::mem::MaybeUninit;

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

#[cfg(feature = "nightly")]
impl IoBuf for Box<[u8]> {
  fn as_slice(&self) -> &[u8] {
    self
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
