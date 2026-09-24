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
/// It provides a pointer to the data and its length.
///
/// # Safety
///
/// The pointer must be non-null and aligned even when empty. Its first
/// len() bytes must be initialized and readable in a live allocation (an empty
/// region may use a non-null dangling pointer). Pointer and length views must
/// agree. While an operation owns this buffer and depends on raw access, storage
/// must remain valid and stable across owner moves. No independently accessible
/// safe alias may invalidate storage or mutate bytes during raw reads. Ordinary
/// safe owner mutation is allowed when no dependent raw access exists.
///
/// ```compile_fail,E0200
/// use lio::IoBuf;
/// struct Buffer(Vec<u8>);
/// impl IoBuf for Buffer {
///  fn as_ptr(&self) -> *const u8 { self.0.as_ptr() }
///  fn len(&self) -> usize { self.0.len() }
/// }
/// ```
pub unsafe trait IoBuf: Send + Sync + 'static {
  /// Returns a pointer to the start of the buffer data.
  fn as_ptr(&self) -> *const u8;

  /// Returns the number of bytes to write.
  fn len(&self) -> usize;

  /// Returns true if the buffer has no data.
  fn is_empty(&self) -> bool {
    self.len() == 0
  }
}

/// A mutable buffer for read operations - provides mutable access and capacity.
///
/// This trait extends [`IoBuf`] for operations that receive data (read, recv, etc.).
/// It provides mutable access to the buffer and the ability to set the length
/// after a read completes.
///
/// # Safety
///
/// In addition to IoBuf, the mutable pointer must describe the same allocation,
/// with len() <= capacity() and the entire capacity writable. Spare capacity need
/// not be initialized. Even at zero capacity the pointer must be non-null and
/// aligned. Storage must remain stable across moves while raw access is pending.
/// No independently accessible safe alias may read, write or invalidate a region
/// during conflicting raw writes. Ordinary safe owner mutation is allowed when
/// no dependent raw access exists. set_len must expose exactly the requested
/// initialized prefix without invalidating storage still depended on.
///
/// ```compile_fail,E0200
/// use lio::{IoBuf, IoBufMut};
/// struct Buffer(Vec<u8>);
/// unsafe impl IoBuf for Buffer {
///  fn as_ptr(&self) -> *const u8 { self.0.as_ptr() }
///  fn len(&self) -> usize { self.0.len() }
/// }
/// impl IoBufMut for Buffer {
///  fn as_mut_ptr(&mut self) -> *mut u8 { self.0.as_mut_ptr() }
///  fn capacity(&self) -> usize { self.0.capacity() }
///  unsafe fn set_len(&mut self, len: usize) { unsafe { self.0.set_len(len) } }
/// }
/// ```
pub unsafe trait IoBufMut: IoBuf {
  /// Returns a mutable pointer to the start of the buffer.
  fn as_mut_ptr(&mut self) -> *mut u8;

  /// Returns the capacity of the buffer (maximum bytes that can be read into it).
  fn capacity(&self) -> usize;

  /// Sets the length of valid data in the buffer.
  ///
  /// Called after a read operation completes to indicate how many bytes were read.
  ///
  /// # Safety
  ///
  /// `len <= capacity()` and the entire new prefix must already be initialized.
  /// No conflicting in-flight raw access may exist when publishing the prefix,
  /// including access that could race with subsequent safe reads. Initialization
  /// need not have been performed by a kernel.
  ///
  /// ```compile_fail,E0133
  /// use lio::IoBufMut;
  /// let mut buf = vec![0u8; 8];
  /// IoBufMut::set_len(&mut buf, 4);
  /// ```
  unsafe fn set_len(&mut self, len: usize);
}

// SAFETY: Owned storage and the element contracts preserve stable, consistent
// views; distinct mutable elements have disjoint writable regions.
unsafe impl IoBuf for Vec<u8> {
  fn as_ptr(&self) -> *const u8 {
    Vec::as_ptr(self)
  }

  fn len(&self) -> usize {
    Vec::len(self)
  }
}

// SAFETY: Owned storage and the element contracts preserve stable, consistent
// views; distinct mutable elements have disjoint writable regions.
unsafe impl IoBufMut for Vec<u8> {
  fn as_mut_ptr(&mut self) -> *mut u8 {
    Vec::as_mut_ptr(self)
  }

  fn capacity(&self) -> usize {
    Vec::capacity(self)
  }

  unsafe fn set_len(&mut self, len: usize) {
    // SAFETY: The caller guarantees an initialized prefix within capacity and
    // no conflicting in-flight access.
    unsafe { Vec::set_len(self, len) }
  }
}

#[cfg(feature = "nightly")]
// SAFETY: Owned storage and the element contracts preserve stable, consistent
// views; distinct mutable elements have disjoint writable regions.
unsafe impl IoBuf for Box<[u8]> {
  fn as_ptr(&self) -> *const u8 {
    <[u8]>::as_ptr(self)
  }

  fn len(&self) -> usize {
    <[u8]>::len(self)
  }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Vectored I/O Buffer Traits (Scatter/Gather)
// ═══════════════════════════════════════════════════════════════════════════════

/// A collection of buffers for vectored writes (`writev`).
///
/// Implemented for tuples, arrays, and `Vec<B>`.
///
/// # Safety
///
/// For each index less than buf_count(), buf must return a non-null, aligned
/// pointer to the reported number of initialized, readable bytes in a live
/// allocation; empty regions may use non-null dangling pointers. Count, pointer
/// and length views must agree. Storage must survive owner moves while an
/// operation depends on raw access. No independently accessible safe alias may
/// invalidate storage or mutate bytes during raw reads. Ordinary safe owner
/// mutation is allowed when no dependent raw access exists. Invalid indices may
/// panic but must not cause undefined behavior.
///
/// ```compile_fail,E0200
/// use lio::IoBufVec;
/// struct Buffers(Vec<Vec<u8>>);
/// impl IoBufVec for Buffers {
///  fn buf_count(&self) -> usize { self.0.len() }
///  fn buf(&self, i: usize) -> (*const u8, usize) { (self.0[i].as_ptr(), self.0[i].len()) }
/// }
/// ```
pub unsafe trait IoBufVec {
  /// Returns the number of buffers in the collection.
  fn buf_count(&self) -> usize;

  /// Returns (ptr, len) for buffer at index `i`.
  fn buf(&self, i: usize) -> (*const u8, usize);
}

/// A collection of mutable buffers for vectored reads (`readv`).
///
/// Implemented for tuples, arrays, and `Vec<B>`.
///
/// # Safety
///
/// For each valid index, buf_mut must return a non-null, aligned pointer to a
/// live allocation writable for the reported capacity; empty regions may use
/// non-null dangling pointers. Simultaneously exposed writable regions must not
/// overlap. Count, capacity, initialized length and immutable views must agree;
/// initialized lengths never exceed capacities. Storage must survive owner moves
/// while an operation depends on raw access. No independently accessible safe
/// alias may read, write or invalidate regions during conflicting raw writes.
/// Ordinary safe owner mutation is allowed when no dependent raw access exists.
/// Invalid indices to the safe accessor may panic but must not cause undefined
/// behavior. set_buf_len must expose exactly the requested initialized prefix of
/// the selected buffer, leaving other buffers and dependent storage valid.
///
/// ```compile_fail,E0200
/// use lio::IoBufMutVec;
/// struct Buffers(Vec<Vec<u8>>);
/// impl IoBufMutVec for Buffers {
///  fn buf_count(&self) -> usize { self.0.len() }
///  fn buf_mut(&mut self, i: usize) -> (*mut u8, usize) {
///  let b = &mut self.0[i]; (b.as_mut_ptr(), b.capacity())
///  }
///  unsafe fn set_buf_len(&mut self, i: usize, len: usize) { unsafe { self.0[i].set_len(len) } }
/// }
/// ```
pub unsafe trait IoBufMutVec: Send + Sync + 'static {
  /// Returns the number of buffers in the collection.
  fn buf_count(&self) -> usize;

  /// Returns (ptr, capacity) for buffer at index `i`.
  fn buf_mut(&mut self, i: usize) -> (*mut u8, usize);

  /// Sets the length of buffer at index `i`.
  ///
  /// # Safety
  ///
  /// `i < buf_count()`, `len` must not exceed that buffer capacity, and its
  /// entire new prefix must already be initialized. No conflicting in-flight
  /// raw access may exist when publishing the prefix, including access that
  /// could race with subsequent safe reads.
  ///
  /// ```compile_fail,E0133
  /// use lio::IoBufMutVec;
  /// let mut bufs = (vec![0u8; 8], vec![0u8; 8]);
  /// bufs.set_buf_len(0, 4);
  /// ```
  unsafe fn set_buf_len(&mut self, i: usize, len: usize);
}

// ═══════════════════════════════════════════════════════════════════════════════
// Single buffer implements vectored traits (a buffer is a 1-element collection)
// ═══════════════════════════════════════════════════════════════════════════════

// SAFETY: Owned storage and the element contracts preserve stable, consistent
// views; distinct mutable elements have disjoint writable regions.
unsafe impl<B: IoBuf> IoBufVec for B {
  fn buf_count(&self) -> usize {
    1
  }
  fn buf(&self, _i: usize) -> (*const u8, usize) {
    (self.as_ptr(), self.len())
  }
}

// SAFETY: Owned storage and the element contracts preserve stable, consistent
// views; distinct mutable elements have disjoint writable regions.
unsafe impl<B: IoBufMut> IoBufMutVec for B {
  fn buf_count(&self) -> usize {
    1
  }
  fn buf_mut(&mut self, _i: usize) -> (*mut u8, usize) {
    (self.as_mut_ptr(), self.capacity())
  }
  unsafe fn set_buf_len(&mut self, _i: usize, len: usize) {
    // SAFETY: The caller supplies the same prefix and access guarantees.
    unsafe { self.set_len(len) };
  }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Tuple implementations for IoBufVec/IoBufMutVec
// ═══════════════════════════════════════════════════════════════════════════════

macro_rules! impl_io_buf_vec_tuple {
  ($count:expr, $($idx:tt: $T:ident),+) => {
    // SAFETY: Owned storage and the element contracts preserve stable, consistent
    // views; distinct mutable elements have disjoint writable regions.
    unsafe impl<$($T: IoBuf),+> IoBufVec for ($($T,)+) {
      fn buf_count(&self) -> usize { $count }

      fn buf(&self, i: usize) -> (*const u8, usize) {
        match i {
          $($idx => (self.$idx.as_ptr(), self.$idx.len()),)+
          _ => panic!("index out of bounds"),
        }
      }
    }

    // SAFETY: Owned storage and the element contracts preserve stable, consistent
    // views; distinct mutable elements have disjoint writable regions.
    unsafe impl<$($T: IoBufMut),+> IoBufMutVec for ($($T,)+) {
      fn buf_count(&self) -> usize { $count }

      fn buf_mut(&mut self, i: usize) -> (*mut u8, usize) {
        match i {
          $($idx => (self.$idx.as_mut_ptr(), self.$idx.capacity()),)+
          _ => panic!("index out of bounds"),
        }
      }

      unsafe fn set_buf_len(&mut self, i: usize, len: usize) {
        match i {
          // SAFETY: The caller guarantees a valid index and initialized prefix.
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
      // SAFETY: Owned storage and the element contracts preserve stable, consistent
      // views; distinct mutable elements have disjoint writable regions.
      unsafe impl<B: IoBuf> IoBufVec for [B; $n] {
        fn buf_count(&self) -> usize { $n }

        fn buf(&self, i: usize) -> (*const u8, usize) {
          (self[i].as_ptr(), self[i].len())
        }
      }

      // SAFETY: Owned storage and the element contracts preserve stable, consistent
      // views; distinct mutable elements have disjoint writable regions.
      unsafe impl<B: IoBufMut> IoBufMutVec for [B; $n] {
        fn buf_count(&self) -> usize { $n }

        fn buf_mut(&mut self, i: usize) -> (*mut u8, usize) {
          (self[i].as_mut_ptr(), self[i].capacity())
        }

        unsafe fn set_buf_len(&mut self, i: usize, len: usize) {
          // SAFETY: The caller guarantees a valid index and initialized prefix.
          unsafe { self[i].set_len(len) };
        }
      }
    )+
  };
}

impl_io_buf_vec_array!(1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16);

// ═══════════════════════════════════════════════════════════════════════════════
// Vec implementations for IoBufVec/IoBufMutVec (dynamic buffer count)
// ═══════════════════════════════════════════════════════════════════════════════

// SAFETY: Owned storage and the element contracts preserve stable, consistent
// views; distinct mutable elements have disjoint writable regions.
unsafe impl<B: IoBuf> IoBufVec for Vec<B> {
  fn buf_count(&self) -> usize {
    self.len()
  }

  fn buf(&self, i: usize) -> (*const u8, usize) {
    (self[i].as_ptr(), self[i].len())
  }
}

// SAFETY: Owned storage and the element contracts preserve stable, consistent
// views; distinct mutable elements have disjoint writable regions.
unsafe impl<B: IoBuf> IoBufVec for &'static mut Vec<B> {
  fn buf_count(&self) -> usize {
    self.len()
  }

  fn buf(&self, i: usize) -> (*const u8, usize) {
    (self[i].as_ptr(), self[i].len())
  }
}

// SAFETY: Owned storage and the element contracts preserve stable, consistent
// views; distinct mutable elements have disjoint writable regions.
unsafe impl<B: IoBufMut> IoBufMutVec for Vec<B> {
  fn buf_count(&self) -> usize {
    self.len()
  }

  fn buf_mut(&mut self, i: usize) -> (*mut u8, usize) {
    (self[i].as_mut_ptr(), self[i].capacity())
  }

  unsafe fn set_buf_len(&mut self, i: usize, len: usize) {
    // SAFETY: The caller guarantees a valid index and initialized prefix.
    unsafe { self[i].set_len(len) };
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn bytes<B: IoBufVec>(bufs: &B, i: usize) -> Vec<u8> {
    let (ptr, len) = bufs.buf(i);
    // SAFETY: IoBufVec guarantees this initialized region, with no pending write.
    unsafe { std::slice::from_raw_parts(ptr, len) }.to_vec()
  }

  #[test]
  fn scalar_initialized_spare_capacity_move_shrink_zero() {
    let mut buf = Vec::with_capacity(12);
    let ptr = IoBufMut::as_mut_ptr(&mut buf);
    // SAFETY: The allocation has at least six writable bytes and no other access.
    unsafe { ptr.copy_from_nonoverlapping(b"abcdef".as_ptr(), 6) };
    let mut moved = std::hint::black_box(buf);
    assert_eq!(ptr, IoBufMut::as_mut_ptr(&mut moved));
    // SAFETY: Six bytes were initialized above; no raw access remains pending.
    unsafe { IoBufMut::set_len(&mut moved, 6) };
    assert_eq!(moved, b"abcdef");
    // SAFETY: Shrinking and zeroing retain only initialized bytes.
    unsafe { IoBufMut::set_len(&mut moved, 2) };
    assert_eq!(moved, b"ab");
    // SAFETY: the empty prefix is initialized and no I/O is pending.
    unsafe { IoBufMut::set_len(&mut moved, 0) };
    assert!(moved.is_empty());
    let mut empty = Vec::<u8>::new();
    // SAFETY: An empty initialized prefix fits even a zero-capacity allocation.
    unsafe { IoBufMut::set_len(&mut empty, 0) };
    assert!(!IoBuf::as_ptr(&empty).is_null());
  }

  fn exercise_vectored<B: IoBufMutVec + IoBufVec>(mut bufs: B) {
    let count = IoBufMutVec::buf_count(&bufs);
    let regions: Vec<_> = (0..count).map(|i| bufs.buf_mut(i)).collect();
    // Move the owner while pointers into its storage are retained.
    let mut moved = std::hint::black_box(bufs);
    for (i, &(ptr, capacity)) in regions.iter().enumerate() {
      assert!(capacity >= 4);
      assert_eq!(ptr, moved.buf_mut(i).0);
      // SAFETY: Each region has four writable bytes, and all regions are disjoint.
      unsafe { ptr.write_bytes(b'a' + i as u8, 4) };
    }
    for i in 0..count {
      // SAFETY: Valid index, four initialized bytes, and all writes have finished.
      unsafe { moved.set_buf_len(i, 4) };
      assert_eq!(bytes(&moved, i), vec![b'a' + i as u8; 4]);
      // SAFETY: Both new lengths are initialized prefixes, with no pending access.
      unsafe { moved.set_buf_len(i, 1) };
      assert_eq!(bytes(&moved, i), vec![b'a' + i as u8]);
      // SAFETY: i is in bounds; the empty prefix is initialized and no I/O is pending.
      unsafe { moved.set_buf_len(i, 0) };
      assert!(bytes(&moved, i).is_empty());
    }
  }

  #[test]
  fn blanket_initialized_spare_capacity() {
    exercise_vectored(Vec::<u8>::with_capacity(8));
  }

  #[test]
  fn tuple_initialized_spare_capacity() {
    exercise_vectored((
      Vec::<u8>::with_capacity(8),
      Vec::<u8>::with_capacity(9),
    ));
  }

  #[test]
  fn array_initialized_spare_capacity() {
    exercise_vectored([
      Vec::<u8>::with_capacity(8),
      Vec::<u8>::with_capacity(9),
    ]);
  }

  #[test]
  fn dynamic_initialized_spare_capacity() {
    exercise_vectored(vec![
      Vec::<u8>::with_capacity(8),
      Vec::<u8>::with_capacity(9),
    ]);
    exercise_vectored(Vec::<Vec<u8>>::new());
  }

  #[cfg(feature = "nightly")]
  #[test]
  fn boxed_initialized_storage_survives_move() {
    let mut buf = Vec::<u8>::with_capacity(8);
    for (slot, byte) in buf.spare_capacity_mut().iter_mut().zip(*b"box") {
      slot.write(byte);
    }
    // SAFETY: The first three bytes were initialized and no raw access is pending.
    unsafe { IoBufMut::set_len(&mut buf, 3) };
    let buf = buf.into_boxed_slice();
    let ptr = IoBuf::as_ptr(&buf);
    let moved = std::hint::black_box(buf);
    assert_eq!(IoBuf::as_ptr(&moved), ptr);
    assert_eq!(bytes(&moved, 0), b"box");
    let empty: Box<[u8]> = Box::default();
    assert!(bytes(&empty, 0).is_empty());
  }
}
