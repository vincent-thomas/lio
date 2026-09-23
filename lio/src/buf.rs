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
/// The pointer must be non-null and valid for reading at least `len()`
/// initialized bytes (dangling is allowed for zero length). The pointer,
/// initialized contents, and length must remain stable while an I/O operation
/// owns the buffer, including across threads. Safe methods must not invalidate
/// the pointer or mutate the data during that time.
///
/// Implementations must be explicitly unsafe:
/// ```compile_fail
/// use lio::IoBuf;
/// struct Fabricated;
/// impl IoBuf for Fabricated {
///     fn as_ptr(&self) -> *const u8 { 1 as *const u8 }
///     fn len(&self) -> usize { 1 }
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
/// `as_mut_ptr()` must be non-null and valid for writing `capacity()` bytes
/// (dangling is allowed for zero capacity). Storage must remain allocated and
/// exclusively accessible while I/O owns the buffer, including across threads;
/// safe methods must not invalidate or alias it, even through another buffer
/// instance. The initialized prefix exposed
/// via `IoBuf` must agree with the length set after I/O.
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
  /// `len <= capacity()` and every byte in the first `len` bytes must already
  /// be initialized. Implementations must expose exactly that initialized prefix.
  unsafe fn set_len(&mut self, len: usize);
}

// SAFETY: Vec owns contiguous initialized elements; ownership keeps its pointer,
// length, and contents stable while I/O holds the Vec.
unsafe impl IoBuf for Vec<u8> {
  fn as_ptr(&self) -> *const u8 {
    Vec::as_ptr(self)
  }

  fn len(&self) -> usize {
    Vec::len(self)
  }
}

// SAFETY: Vec owns writable storage up to capacity without safe aliases during
// I/O ownership; set_len forwards the caller's initialization bound.
unsafe impl IoBufMut for Vec<u8> {
  fn as_mut_ptr(&mut self) -> *mut u8 {
    Vec::as_mut_ptr(self)
  }

  fn capacity(&self) -> usize {
    Vec::capacity(self)
  }

  unsafe fn set_len(&mut self, len: usize) {
    // SAFETY: the unsafe method contract guarantees initialized bytes up to
    // len and len <= capacity.
    unsafe { Vec::set_len(self, len) }
  }
}

#[cfg(feature = "nightly")]
// SAFETY: Box owns its initialized slice; pointer, length and contents remain
// stable while the box is owned by I/O.
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
/// For each index below `buf_count()`, `buf(i)` must return a non-null pointer
/// to `len` initialized, readable bytes (dangling allowed for zero length).
/// Count, pointers, lengths, and contents must stay stable while I/O owns the
/// collection, including across threads. Safe access cannot invalidate them.
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
/// The count, pointers and capacities must stay stable while I/O owns the
/// collection, including across threads. Every `buf_mut(i)` below the count
/// must return a non-null pointer to `capacity` writable bytes (dangling
/// allowed for zero capacity). Segments must be disjoint and exclusively
/// accessible for that time. Safe methods must not invalidate or alias them.
/// `set_buf_len` must expose only initialized bytes.
pub unsafe trait IoBufMutVec: Send + Sync + 'static {
  /// Returns the number of buffers in the collection.
  fn buf_count(&self) -> usize;

  /// Returns (ptr, capacity) for buffer at index `i`.
  fn buf_mut(&mut self, i: usize) -> (*mut u8, usize);

  /// Sets the length of buffer at index `i`.
  ///
  /// # Safety
  /// `i < buf_count()`, `len <= buf_mut(i).1`, and the first `len` bytes
  /// of that segment must already be initialized. The implementation must
  /// expose exactly that initialized prefix without changing other segments.
  unsafe fn set_buf_len(&mut self, i: usize, len: usize);
}

// ═══════════════════════════════════════════════════════════════════════════════
// Single buffer implements vectored traits (a buffer is a 1-element collection)
// ═══════════════════════════════════════════════════════════════════════════════

// SAFETY: The sole segment inherits IoBuf's stable pointer and initialized length.
unsafe impl<B: IoBuf> IoBufVec for B {
  fn buf_count(&self) -> usize {
    1
  }
  fn buf(&self, _i: usize) -> (*const u8, usize) {
    (self.as_ptr(), self.len())
  }
}

// SAFETY: The sole segment inherits IoBufMut's exclusive writable storage and
// stable capacity; set_buf_len delegates the caller's initialization bound.
unsafe impl<B: IoBufMut> IoBufMutVec for B {
  fn buf_count(&self) -> usize {
    1
  }
  fn buf_mut(&mut self, _i: usize) -> (*mut u8, usize) {
    (self.as_mut_ptr(), self.capacity())
  }
  unsafe fn set_buf_len(&mut self, _i: usize, len: usize) {
    // SAFETY: The caller guarantees len initialized bytes within the sole
    // segment's capacity.
    unsafe { self.set_len(len) };
  }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Tuple implementations for IoBufVec/IoBufMutVec
// ═══════════════════════════════════════════════════════════════════════════════

macro_rules! impl_io_buf_vec_tuple {
  ($count:expr, $($idx:tt: $T:ident),+) => {
    // SAFETY: The fixed count indexes tuple fields, each of which supplies
    // stable initialized storage under its IoBuf contract.
    unsafe impl<$($T: IoBuf),+> IoBufVec for ($($T,)+) {
      fn buf_count(&self) -> usize { $count }

      fn buf(&self, i: usize) -> (*const u8, usize) {
        match i {
          $($idx => (self.$idx.as_ptr(), self.$idx.len()),)+
          _ => panic!("index out of bounds"),
        }
      }
    }

    // SAFETY: Distinct tuple fields have disjoint storage; each IoBufMut
    // supplies stable, exclusively writable capacity while I/O owns the tuple.
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
          // SAFETY: The caller guarantees the indexed field has len initialized
          // bytes within that field's capacity.
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
      // SAFETY: Array count is fixed and each indexed IoBuf provides stable,
      // initialized storage while I/O owns the array.
      unsafe impl<B: IoBuf> IoBufVec for [B; $n] {
        fn buf_count(&self) -> usize { $n }

        fn buf(&self, i: usize) -> (*const u8, usize) {
          (self[i].as_ptr(), self[i].len())
        }
      }

      // SAFETY: Separate array elements have disjoint storage and each
      // IoBufMut provides stable, exclusively writable capacity.
      unsafe impl<B: IoBufMut> IoBufMutVec for [B; $n] {
        fn buf_count(&self) -> usize { $n }

        fn buf_mut(&mut self, i: usize) -> (*mut u8, usize) {
          (self[i].as_mut_ptr(), self[i].capacity())
        }

        unsafe fn set_buf_len(&mut self, i: usize, len: usize) {
          // SAFETY: The caller guarantees i is in bounds and len initialized
          // bytes fit in this element's capacity.
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

// SAFETY: Ownership prevents safe changes to Vec's element count during I/O;
// each IoBuf provides stable initialized storage.
unsafe impl<B: IoBuf> IoBufVec for Vec<B> {
  fn buf_count(&self) -> usize {
    self.len()
  }

  fn buf(&self, i: usize) -> (*const u8, usize) {
    (self[i].as_ptr(), self[i].len())
  }
}

// SAFETY: The exclusive mutable reference prevents safe changes to the Vec
// during I/O; each IoBuf element supplies stable initialized storage.
unsafe impl<B: IoBuf> IoBufVec for &'static mut Vec<B> {
  fn buf_count(&self) -> usize {
    self.len()
  }

  fn buf(&self, i: usize) -> (*const u8, usize) {
    (self[i].as_ptr(), self[i].len())
  }
}

// SAFETY: Vec ownership fixes the count during I/O, distinct elements have
// disjoint storage, and each IoBufMut supplies stable writable capacity.
unsafe impl<B: IoBufMut> IoBufMutVec for Vec<B> {
  fn buf_count(&self) -> usize {
    self.len()
  }

  fn buf_mut(&mut self, i: usize) -> (*mut u8, usize) {
    (self[i].as_mut_ptr(), self[i].capacity())
  }

  unsafe fn set_buf_len(&mut self, i: usize, len: usize) {
    // SAFETY: The caller guarantees i is in bounds and len initialized
    // bytes fit in this element's capacity.
    unsafe { self[i].set_len(len) };
  }
}
