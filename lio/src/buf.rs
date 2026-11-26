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
//! it does not own. After a read, lio passes an [`Initialized`] proof to
//! [`IoBufMut::commit`]; that proof is the only way to publish the bytes the
//! kernel wrote.
//!
//! # Constants
//!
//! - [`MAX_IOV_COUNT`]: Maximum number of buffers for vectored I/O (16).

use std::marker::PhantomData;
use std::mem::MaybeUninit;

/// Proof that the first [`len`](Self::len) bytes of one buffer region were
/// initialized.
///
/// Only lio mints this, after a read has written those bytes. Applying it to
/// a [`Vec<u8>`](Initialized::apply_to_vec) is safe: the proof cannot be
/// forged, and it is rejected unless it names that `Vec`'s allocation.
pub struct Initialized<'a> {
  ptr: *mut u8,
  len: usize,
  region: usize,
  _lt: PhantomData<&'a mut [MaybeUninit<u8>]>,
}

impl<'a> Initialized<'a> {
  pub(crate) fn new(ptr: *mut u8, region: usize, len: usize) -> Self {
    assert!(
      len <= region,
      "initialized length {len} exceeds the buffer region {region}"
    );
    Self { ptr, len, region, _lt: PhantomData }
  }

  /// How many bytes were initialized.
  pub fn len(&self) -> usize {
    self.len
  }

  /// Whether no bytes were initialized.
  pub fn is_empty(&self) -> bool {
    self.len == 0
  }

  /// The region this proof describes.
  pub fn region_len(&self) -> usize {
    self.region
  }

  /// Start of the initialized region.
  pub fn as_ptr(&self) -> *mut u8 {
    self.ptr
  }

  /// Publishes `self` as `vec`'s initialized length.
  ///
  /// # Panics
  ///
  /// Panics if `self` does not describe `vec`'s current allocation, or if the
  /// initialized length exceeds `vec`'s capacity. A proof minted for one
  /// buffer cannot resize another.
  pub fn apply_to_vec(self, vec: &mut Vec<u8>) {
    assert!(
      vec.as_mut_ptr() == self.ptr && self.len <= vec.capacity(),
      "initialized proof does not match this Vec"
    );
    // SAFETY: this proof was minted for this allocation after `len` bytes
    // were initialized, and `len` is within capacity. The exclusive borrow
    // keeps the allocation in place, so the pointer check identifies it.
    unsafe { vec.set_len(self.len) }
  }
}

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
/// `&self` methods on it in that time.
///
/// # Safety
///
/// `as_slice` returns bytes that stay at that address until the buffer is
/// dropped or the operation completes. A shared borrow does not reallocate
/// or move them.
pub unsafe trait IoBuf: Send + Sync + 'static {
  /// Returns the bytes to write.
  fn as_slice(&self) -> &[u8];
}

/// A mutable buffer for read operations - provides mutable access and capacity.
///
/// This trait extends [`IoBuf`] for operations that receive data (read, recv, etc.).
/// It provides mutable access to the buffer's whole capacity and the ability to
/// set the length after a read completes.
///
/// # Safety
///
/// `as_uninit_mut` is the whole writable region. Its prefix of length
/// [`as_slice().len()`](IoBuf::as_slice) is initialized. `commit` publishes
/// only the prefix [`Initialized`] proves.
pub unsafe trait IoBufMut: IoBuf {
  /// Returns the entire writable region of the buffer, from its start up to
  /// its capacity. Reads fill this region from the beginning.
  ///
  /// The first [`as_slice().len()`](IoBuf::as_slice) elements are initialized;
  /// the rest may not be.
  fn as_uninit_mut(&mut self) -> &mut [MaybeUninit<u8>];

  /// Publishes `initialized` as the buffer's valid prefix.
  ///
  /// lio calls this after a read with a proof for the region
  /// [`as_uninit_mut`](IoBufMut::as_uninit_mut) returned. Implementors that
  /// store bytes in a [`Vec<u8>`] should call
  /// [`Initialized::apply_to_vec`].
  fn commit(&mut self, initialized: Initialized<'_>);
}

// SAFETY: `Vec<u8>` does not move its allocation under a shared borrow.
unsafe impl IoBuf for Vec<u8> {
  fn as_slice(&self) -> &[u8] {
    self
  }
}

// SAFETY: `as_uninit_mut` covers the allocation. `commit` accepts only a
// proof for that allocation.
unsafe impl IoBufMut for Vec<u8> {
  fn as_uninit_mut(&mut self) -> &mut [MaybeUninit<u8>] {
    let cap = self.capacity();
    // SAFETY: the allocation holds `cap` bytes, and `MaybeUninit<u8>` permits
    // uninitialized contents. The borrow of `self` prevents reallocation.
    unsafe { std::slice::from_raw_parts_mut(self.as_mut_ptr().cast(), cap) }
  }

  fn commit(&mut self, initialized: Initialized<'_>) {
    initialized.apply_to_vec(self);
  }
}

// SAFETY: a boxed slice does not move its allocation under a shared borrow.
unsafe impl IoBuf for Box<[u8]> {
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
///
/// # Safety
///
/// Every buffer `buf` returns stays at that address while this collection is
/// owned by an in-flight operation.
pub unsafe trait IoBufVec {
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
  /// Only the first `len` are written: a single buffer, the common case,
  /// should not pay for the other fifteen.
  slices: [MaybeUninit<&'a mut [MaybeUninit<u8>]>; MAX_IOV_COUNT],
  len: usize,
}

impl<'a> IoSlicesMut<'a> {
  /// Creates an empty set of regions.
  pub fn new() -> Self {
    Self { slices: [const { MaybeUninit::uninit() }; MAX_IOV_COUNT], len: 0 }
  }

  /// Appends a region. Ignored once [`MAX_IOV_COUNT`] regions are held.
  pub fn push(&mut self, region: &'a mut [MaybeUninit<u8>]) {
    if let Some(slot) = self.slices.get_mut(self.len) {
      slot.write(region);
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
  type IntoIter = IoSlicesMutIter<'a>;

  fn into_iter(self) -> Self::IntoIter {
    IoSlicesMutIter { regions: self, next: 0 }
  }
}

/// The regions of an [`IoSlicesMut`], in the order they were pushed.
pub struct IoSlicesMutIter<'a> {
  regions: IoSlicesMut<'a>,
  next: usize,
}

impl<'a> Iterator for IoSlicesMutIter<'a> {
  type Item = &'a mut [MaybeUninit<u8>];

  fn next(&mut self) -> Option<Self::Item> {
    if self.next >= self.regions.len {
      return None;
    }
    let slot = self.regions.slices.get(self.next)?;
    self.next += 1;
    // SAFETY: `push` wrote the first `len` slots. `next` only moves forward
    // and nothing else reads a slot (the type is neither `Clone` nor has a
    // `Drop` that would), so each reference is moved out exactly once. The
    // bytes left behind in the `MaybeUninit` are never used as a reference
    // again, as with `std::array::IntoIter`.
    Some(unsafe { slot.assume_init_read() })
  }

  fn size_hint(&self) -> (usize, Option<usize>) {
    let left = self.regions.len - self.next;
    (left, Some(left))
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
/// # Safety
///
/// `bufs_mut` yields every region, and they stay at those addresses until the
/// operation completes. `commit` publishes only the prefix the proof names.
///
/// # Example
///
/// A header/body pair. The impl is `unsafe` because it promises those
/// regions stay put for the whole operation:
///
/// ```
/// use lio::{IoBufMut, IoBufMutVec, IoSlicesMut};
///
/// struct Frame {
///   header: Vec<u8>,
///   body: Vec<u8>,
/// }
///
/// unsafe impl IoBufMutVec for Frame {
///   fn bufs_mut<'a>(&'a mut self, out: &mut IoSlicesMut<'a>) {
///     out.push(self.header.as_uninit_mut());
///     out.push(self.body.as_uninit_mut());
///   }
///
///   fn commit(&mut self, index: usize, initialized: lio::Initialized<'_>) {
///     let buf = if index == 0 { &mut self.header } else { &mut self.body };
///     initialized.apply_to_vec(buf);
///   }
/// }
///
/// let mut frame =
///   Frame { header: Vec::with_capacity(4), body: Vec::with_capacity(16) };
/// let mut regions = IoSlicesMut::new();
/// frame.bufs_mut(&mut regions);
/// assert_eq!(regions.len(), 2);
/// ```
pub unsafe trait IoBufMutVec: Send + Sync + 'static {
  /// Pushes the writable region of each buffer, in order, as
  /// [`IoBufMut::as_uninit_mut`] does for a single buffer.
  fn bufs_mut<'a>(&'a mut self, out: &mut IoSlicesMut<'a>);

  /// Publishes `initialized` as the length of the buffer at `index`.
  ///
  /// `index` is the position of that buffer in the most recent
  /// [`bufs_mut`](IoBufMutVec::bufs_mut) call. The proof describes that
  /// region.
  fn commit(&mut self, index: usize, initialized: Initialized<'_>);
}

// ═══════════════════════════════════════════════════════════════════════════════
// Single buffer implements vectored traits (a buffer is a 1-element collection)
// ═══════════════════════════════════════════════════════════════════════════════

// SAFETY: the buffer's `IoBuf` promise covers this one-element collection.
unsafe impl<B: IoBuf> IoBufVec for B {
  fn buf_count(&self) -> usize {
    1
  }
  fn buf(&self, _i: usize) -> &[u8] {
    self.as_slice()
  }
}

// SAFETY: the buffer's `IoBufMut` promise covers this one-element collection.
unsafe impl<B: IoBufMut> IoBufMutVec for B {
  fn bufs_mut<'a>(&'a mut self, out: &mut IoSlicesMut<'a>) {
    out.push(self.as_uninit_mut());
  }
  fn commit(&mut self, _index: usize, initialized: Initialized<'_>) {
    IoBufMut::commit(self, initialized);
  }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Tuple implementations for IoBufVec/IoBufMutVec
// ═══════════════════════════════════════════════════════════════════════════════

macro_rules! impl_io_buf_vec_tuple {
  ($count:expr, $($idx:tt: $T:ident),+) => {
    // SAFETY: each element is an `IoBuf`, so its bytes stay put.
    unsafe impl<$($T: IoBuf),+> IoBufVec for ($($T,)+) {
      fn buf_count(&self) -> usize { $count }

      fn buf(&self, i: usize) -> &[u8] {
        match i {
          $($idx => self.$idx.as_slice(),)+
          _ => panic!("index out of bounds"),
        }
      }
    }

    // SAFETY: each element is an `IoBufMut`, so its region and `commit` stay honest.
    unsafe impl<$($T: IoBufMut),+> IoBufMutVec for ($($T,)+) {
      fn bufs_mut<'a>(&'a mut self, out: &mut IoSlicesMut<'a>) {
        $(out.push(self.$idx.as_uninit_mut());)+
      }

      fn commit(&mut self, index: usize, initialized: Initialized<'_>) {
        match index {
          $($idx => IoBufMut::commit(&mut self.$idx, initialized),)+
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
      // SAFETY: each element is an `IoBuf`, so its bytes stay put.
      unsafe impl<B: IoBuf> IoBufVec for [B; $n] {
        fn buf_count(&self) -> usize { $n }

        fn buf(&self, i: usize) -> &[u8] {
          self[i].as_slice()
        }
      }

      // SAFETY: each element is an `IoBufMut`, so its region and `commit` stay honest.
      unsafe impl<B: IoBufMut> IoBufMutVec for [B; $n] {
        fn bufs_mut<'a>(&'a mut self, out: &mut IoSlicesMut<'a>) {
          for buf in self {
            out.push(buf.as_uninit_mut());
          }
        }

        fn commit(&mut self, index: usize, initialized: Initialized<'_>) {
          IoBufMut::commit(&mut self[index], initialized);
        }
      }
    )+
  };
}

impl_io_buf_vec_array!(1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16);

// ═══════════════════════════════════════════════════════════════════════════════
// Vec implementations for IoBufVec/IoBufMutVec (dynamic buffer count)
// ═══════════════════════════════════════════════════════════════════════════════

// SAFETY: each element is an `IoBuf`, so its bytes stay put.
unsafe impl<B: IoBuf> IoBufVec for Vec<B> {
  fn buf_count(&self) -> usize {
    self.len()
  }

  fn buf(&self, i: usize) -> &[u8] {
    self[i].as_slice()
  }
}

// SAFETY: each element is an `IoBuf`, so its bytes stay put.
unsafe impl<B: IoBuf> IoBufVec for &'static mut Vec<B> {
  fn buf_count(&self) -> usize {
    self.len()
  }

  fn buf(&self, i: usize) -> &[u8] {
    self[i].as_slice()
  }
}

// SAFETY: each element is an `IoBufMut`, so its region and `commit` stay honest.
unsafe impl<B: IoBufMut> IoBufMutVec for Vec<B> {
  fn bufs_mut<'a>(&'a mut self, out: &mut IoSlicesMut<'a>) {
    for buf in self {
      out.push(buf.as_uninit_mut());
    }
  }

  fn commit(&mut self, index: usize, initialized: Initialized<'_>) {
    IoBufMut::commit(&mut self[index], initialized);
  }
}

#[cfg(test)]
mod tests {
  use super::Initialized;

  #[test]
  fn proof_sets_the_length_of_its_own_vec() {
    let mut bytes = vec![9u8; 4];
    bytes.clear();
    let proof = Initialized::new(bytes.as_mut_ptr(), bytes.capacity(), 2);
    proof.apply_to_vec(&mut bytes);
    assert_eq!(bytes, [9, 9]);
  }

  #[test]
  #[cfg_attr(not(panic = "unwind"), ignore = "catches a panic")]
  fn proof_rejects_a_different_allocation() {
    let mut owned = vec![1u8, 2, 3];
    let mut other = vec![0u8; 3];
    let proof = Initialized::new(owned.as_mut_ptr(), owned.capacity(), 2);
    let panicked =
      std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        proof.apply_to_vec(&mut other);
      }));
    assert!(panicked.is_err());
    assert_eq!(other, [0, 0, 0]);
  }
}
