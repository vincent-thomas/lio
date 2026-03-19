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
pub trait IoBuf: Send + Sync + 'static {
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
pub trait IoBufMut: IoBuf {
  /// Returns a mutable pointer to the start of the buffer.
  fn as_mut_ptr(&mut self) -> *mut u8;

  /// Returns the capacity of the buffer (maximum bytes that can be read into it).
  fn capacity(&self) -> usize;

  /// Sets the length of valid data in the buffer.
  ///
  /// Called after a read operation completes to indicate how many bytes were read.
  ///
  /// The caller must ensure that `len <= capacity()` and that the first `len` bytes
  /// have been initialized by the kernel.
  fn set_len(&mut self, len: usize);
}

impl IoBuf for Vec<u8> {
  fn as_ptr(&self) -> *const u8 {
    Vec::as_ptr(self)
  }

  fn len(&self) -> usize {
    Vec::len(self)
  }
}

impl IoBufMut for Vec<u8> {
  fn as_mut_ptr(&mut self) -> *mut u8 {
    Vec::as_mut_ptr(self)
  }

  fn capacity(&self) -> usize {
    Vec::capacity(self)
  }

  fn set_len(&mut self, len: usize) {
    // SAFETY: `len` comes from the kernel indicating how many bytes were written
    // into the buffer. The caller guarantees len <= capacity.
    unsafe { Vec::set_len(self, len) }
  }
}

#[cfg(feature = "nightly")]
impl IoBuf for Box<[u8]> {
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
pub trait IoBufVec: Send + Sync + 'static {
  /// Returns the number of buffers in the collection.
  fn buf_count(&self) -> usize;

  /// Returns (ptr, len) for buffer at index `i`.
  fn buf(&self, i: usize) -> (*const u8, usize);
}

/// A collection of mutable buffers for vectored reads (`readv`).
///
/// Implemented for tuples, arrays, and `Vec<B>`.
pub trait IoBufMutVec: Send + Sync + 'static {
  /// Returns the number of buffers in the collection.
  fn buf_count(&self) -> usize;

  /// Returns (ptr, capacity) for buffer at index `i`.
  fn buf_mut(&mut self, i: usize) -> (*mut u8, usize);

  /// Sets the length of buffer at index `i`.
  fn set_buf_len(&mut self, i: usize, len: usize);
}

// ═══════════════════════════════════════════════════════════════════════════════
// Single buffer implements vectored traits (a buffer is a 1-element collection)
// ═══════════════════════════════════════════════════════════════════════════════

impl<B: IoBuf> IoBufVec for B {
  fn buf_count(&self) -> usize {
    1
  }
  fn buf(&self, _i: usize) -> (*const u8, usize) {
    (self.as_ptr(), self.len())
  }
}

impl<B: IoBufMut> IoBufMutVec for B {
  fn buf_count(&self) -> usize {
    1
  }
  fn buf_mut(&mut self, _i: usize) -> (*mut u8, usize) {
    (self.as_mut_ptr(), self.capacity())
  }
  fn set_buf_len(&mut self, _i: usize, len: usize) {
    self.set_len(len);
  }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Tuple implementations for IoBufVec/IoBufMutVec
// ═══════════════════════════════════════════════════════════════════════════════

macro_rules! impl_io_buf_vec_tuple {
  ($count:expr, $($idx:tt: $T:ident),+) => {
    impl<$($T: IoBuf),+> IoBufVec for ($($T,)+) {
      fn buf_count(&self) -> usize { $count }

      fn buf(&self, i: usize) -> (*const u8, usize) {
        match i {
          $($idx => (self.$idx.as_ptr(), self.$idx.len()),)+
          _ => panic!("index out of bounds"),
        }
      }
    }

    impl<$($T: IoBufMut),+> IoBufMutVec for ($($T,)+) {
      fn buf_count(&self) -> usize { $count }

      fn buf_mut(&mut self, i: usize) -> (*mut u8, usize) {
        match i {
          $($idx => (self.$idx.as_mut_ptr(), self.$idx.capacity()),)+
          _ => panic!("index out of bounds"),
        }
      }

      fn set_buf_len(&mut self, i: usize, len: usize) {
        match i {
          $($idx => self.$idx.set_len(len),)+
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

        fn buf(&self, i: usize) -> (*const u8, usize) {
          (self[i].as_ptr(), self[i].len())
        }
      }

      impl<B: IoBufMut> IoBufMutVec for [B; $n] {
        fn buf_count(&self) -> usize { $n }

        fn buf_mut(&mut self, i: usize) -> (*mut u8, usize) {
          (self[i].as_mut_ptr(), self[i].capacity())
        }

        fn set_buf_len(&mut self, i: usize, len: usize) {
          self[i].set_len(len);
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

  fn buf(&self, i: usize) -> (*const u8, usize) {
    (self[i].as_ptr(), self[i].len())
  }
}

impl<B: IoBufMut> IoBufMutVec for Vec<B> {
  fn buf_count(&self) -> usize {
    self.len()
  }

  fn buf_mut(&mut self, i: usize) -> (*mut u8, usize) {
    (self[i].as_mut_ptr(), self[i].capacity())
  }

  fn set_buf_len(&mut self, i: usize, len: usize) {
    self[i].set_len(len);
  }
}
