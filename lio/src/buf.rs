//! Buffer abstractions for zero-copy I/O operations.
//!
//! This module provides buffer types and traits for efficient I/O without unnecessary
//! allocations. The core abstraction is [`BufLike`], which manages the lifecycle of
//! buffers used in syscalls.
//!
//! # Buffer Lifecycle
//!
//! Buffers go through a three-phase lifecycle:
//!
//! 1. **Init**: [`BufLike::init`] prepares the buffer for I/O (e.g., clears length)
//! 2. **Use**: [`BufLike::buf_ptr`] and [`BufLike::buf_len`] provide pointer and capacity for syscalls
//! 3. **Deinit**: [`BufLike::deinit`] updates buffer state with bytes transferred
//!
//! # Buffer Pool
//!
//! The [`BufStore`] (requires `buf` feature) provides a pool of reusable 4096-byte buffers
//! to avoid heap allocations. Each buffer is protected by a mutex for concurrent access.
//!
//! ```ignore
//! let pool = BufStore::default(); // 128 buffers
//!
//! if let Some(buf) = pool.try_get() {
//!     // Got pooled buffer, no allocation
//! } else {
//!     // All buffers in use, fallback to Vec::with_capacity(4096)
//! }
//! ```
//!
//! # Design
//! In 99 times out of 100, the [`Buf`] wrapper should be used instead of
//! dealing with the raw [`BufLike`], as it manages the init/deinit lifecycle
//! automatically.

/// Result type for operations that return both a result and a buffer.
///
/// This is commonly used for read/write operations where the buffer
/// is returned along with the operation result.
///
/// Example: See [`lio::read`](crate::read).
pub type BufResult<T, B> = (std::io::Result<T>, B);

trait Sealed {}

/// A interface over buffers.
///
/// # Rules of implementation
/// This trait assumes there are three parts of the implementor.
/// - Pointer starting at the memory allocated.
/// - Capacity: Total length of memory allocated.
/// - Length: Total length of valid data.
///
/// The implementor should always set 'Length' to be the same as bytes written or bytes read in
/// [`BufLike::deinit`].
pub trait BufLike: Sealed {
  /// Called before the buffer is used for I/O operations.
  ///
  /// For example this could be used for clearing the buffer from old data.
  fn init(&mut self);

  /// Returns a pointer to the buffer data.
  ///
  /// # Safety
  ///
  /// The returned pointer must be valid for [`buf_len()`](BufLike::buf_len) bytes
  /// and must not be invalidated during the I/O operation.
  unsafe fn buf_ptr(&self) -> *const u8;

  /// Returns the buffer length/capacity.
  ///
  /// - For reads: this is the maximum number of bytes that can be read.
  /// - For writes: this is the number of bytes to write.
  ///
  /// # Safety
  ///
  /// The pointer + length must be allocated memory for the entire length of
  /// the I/O operation.
  unsafe fn buf_len(&self) -> usize;

  /// Called after I/O completes with the number of bytes transferred.
  ///
  /// For example this could be used for setting the "real" length of the buffer.
  ///
  /// # Parameters
  ///
  /// - `bytes`: bytes written (for writes) or bytes read (for reads)
  fn deinit(&mut self, bytes: usize);
}

impl BufLike for Vec<u8> {
  fn init(&mut self) {}

  unsafe fn buf_ptr(&self) -> *const u8 {
    self.as_ptr()
  }

  unsafe fn buf_len(&self) -> usize {
    self.capacity()
  }

  fn deinit(&mut self, bw: usize) {
    unsafe { self.set_len(bw) };
  }
}

impl<B> Sealed for B where B: BufLike {}

/// Wrapper around a [`BufLike`] buffer for I/O operations.
///
/// This newtype wraps a buffer implementing [`BufLike`] and manages its lifecycle
/// through init/deinit calls.
///
/// # Example
///
/// ```ignore
/// let buf = Buf::new(Vec::with_capacity(1024));
/// let (ptr, len) = buf.get(); // Get buffer pointer and length for syscall
/// // ... perform I/O ...
/// let vec = buf.after(bytes_transferred); // Update state and extract buffer
/// ```
pub struct Buf<T>(T);

impl<B> Buf<B>
where
  B: BufLike,
{
  /// Creates a new `Buf` wrapping the given buffer.
  ///
  /// Calls `init()` on the buffer to prepare it for I/O.
  pub fn new(mut buf_like: B) -> Self {
    buf_like.init();
    Buf(buf_like)
  }

  /// Returns the buffer pointer and length for use in syscalls.
  ///
  /// # Returns
  ///
  /// `(*const u8, usize)`: Pointer to buffer data and its length
  pub fn get(&self) -> (*const u8, usize) {
    (unsafe { self.0.buf_ptr() }, unsafe { self.0.buf_len() })
  }

  /// Notifies the buffer that I/O has completed and extracts it.
  ///
  /// Calls `deinit(bw)` on the buffer to update its state based on
  /// the number of bytes transferred, then returns the inner buffer.
  ///
  /// # Parameters
  ///
  /// - `bw`: bytes written (for writes) or bytes read (for reads)
  ///
  /// # Returns
  ///
  /// The inner buffer with its state updated
  pub fn after(mut self, bw: usize) -> B {
    self.0.deinit(bw);
    self.0
  }
}

#[cfg(feature = "buf")]
pub use extra_buf::*;

#[cfg(feature = "buf")]
mod extra_buf {
  use crate::sync::Mutex;
  use std::array;

  /// A borrowed buffer from a `BufStore` pool.
  ///
  /// This buffer is automatically returned to the pool when dropped.
  /// It implements `BufLike` and can be used for I/O operations without
  /// allocating new memory.
  ///
  /// **Make sure to drop**. Doing any [`std::mem::forget`] will lead to bad things.
  pub struct LentBuf<'a>(crate::sync::MutexGuard<'a, BufStoreBuffer>);

  impl super::BufLike for LentBuf<'_> {
    fn init(&mut self) {
      self.0.len = 0;
      #[cfg(feature = "buf")]
      {
        self.0.pos = 0;
      }
    }

    unsafe fn buf_ptr(&self) -> *const u8 {
      self.0.buf.as_ptr()
    }

    unsafe fn buf_len(&self) -> usize {
      self.0.buf.len()
    }

    fn deinit(&mut self, bw: usize) {
      assert!(
        bw <= self.0.buf.len(),
        "LentBuf::deinit: bytes written ({}) exceeds buffer capacity ({})",
        bw,
        self.0.buf.len()
      );
      self.0.len = bw;
    }
  }

  impl<'a> AsRef<[u8]> for LentBuf<'a> {
    fn as_ref(&self) -> &[u8] {
      &self.0.buf[self.pos()..self.0.len]
    }
  }

  pub struct LentBufIter<'a> {
    buf: &'a LentBuf<'a>,
    index: usize,
  }

  impl<'a> Iterator for LentBufIter<'a> {
    type Item = u8;
    fn next(&mut self) -> Option<Self::Item> {
      let remaining = self.buf.0.len - self.buf.pos();
      if self.index >= remaining {
        None
      } else {
        let val = self.buf.as_ref()[self.index];
        self.index += 1;
        Some(val)
      }
    }
  }

  impl<'a> IntoIterator for &'a LentBuf<'a> {
    type Item = u8;
    type IntoIter = LentBufIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
      LentBufIter { buf: self, index: 0 }
    }
  }

  /// Implementation of `bytes::Buf` for `LentBuf`.
  ///
  /// Allows `LentBuf` to be used with the `bytes` crate ecosystem.
  /// The buffer's valid data is from `pos..len`, where `len` is set by `deinit()`
  /// and `pos` tracks how much has been consumed via `advance()`.
  ///
  /// This implementation uses a position offset instead of copying data,
  /// making `advance()` a zero-copy operation.
  ///
  /// # Example
  ///
  /// ```ignore
  /// use bytes::Buf;
  ///
  /// let store = BufStore::default();
  /// let mut buf = store.try_get().unwrap();
  /// // ... fill buffer with data and call deinit(n) ...
  ///
  /// // Use as bytes::Buf - all operations are zero-copy
  /// let byte = buf.get_u8();
  /// let bytes = buf.copy_to_bytes(4);
  /// ```
  impl bytes::Buf for LentBuf<'_> {
    fn remaining(&self) -> usize {
      self.0.len - self.0.pos
    }

    fn chunk(&self) -> &[u8] {
      &self.0.buf[self.0.pos..self.0.len]
    }

    fn advance(&mut self, cnt: usize) {
      let remaining = self.0.len - self.0.pos;
      assert!(
        cnt <= remaining,
        "LentBuf::advance: cannot advance by {} bytes, only {} remaining",
        cnt,
        remaining
      );

      self.0.pos += cnt;
    }
  }

  impl<'a> LentBuf<'a> {
    #[inline]
    fn pos(&self) -> usize {
      #[cfg(feature = "buf")]
      {
        self.0.pos
      }
      #[cfg(not(feature = "buf"))]
      {
        0
      }
    }

    /// Returns an iterator over `chunk_size` chunks of the buffer.
    ///
    /// The last chunk may be shorter if the buffer length is not evenly divisible.
    ///
    /// # Panics
    ///
    /// Panics if `chunk_size` is 0.
    pub fn chunks(&'a self, chunk_size: usize) -> std::slice::Chunks<'a, u8> {
      self.as_ref().chunks(chunk_size)
    }

    /// Returns an iterator over overlapping windows of `window_size`.
    ///
    /// Each window overlaps with the previous by `window_size - 1` bytes.
    ///
    /// # Panics
    ///
    /// Panics if `window_size` is 0 or greater than buffer length.
    pub fn windows(
      &'a self,
      window_size: usize,
    ) -> std::slice::Windows<'a, u8> {
      self.as_ref().windows(window_size)
    }
  }

  pub(crate) struct BufStoreBuffer {
    buf: [u8; 4096],
    len: usize,
    #[cfg(feature = "buf")]
    pos: usize,
  }

  impl BufStoreBuffer {
    pub fn new() -> Self {
      Self {
        buf: array::from_fn::<_, 4096, _>(|_| 0),
        len: 0,
        #[cfg(feature = "buf")]
        pos: 0,
      }
    }
  }

  /// A pool of reusable buffers for I/O operations.
  ///
  /// Provides a way to avoid heap allocations by reusing fixed-size buffers.
  /// Each buffer is 4096 bytes and protected by a `Mutex` to allow concurrent access.
  ///
  /// # Example
  ///
  /// ```ignore
  /// let buf_store = BufStore::with_capacity(64); // 64 buffers in pool
  ///
  /// if let Some(buf) = buf_store.try_get() {
  ///     // Got a buffer from pool, use for I/O
  /// } else {
  ///     // All buffers in use, fallback to Vec or wait
  /// }
  /// ```
  pub struct BufStore {
    store: Box<[Mutex<BufStoreBuffer>]>,
  }

  impl Default for BufStore {
    fn default() -> Self {
      Self::with_capacity(128)
    }
  }

  impl BufStore {
    /// Creates a new buffer pool with the specified capacity.
    ///
    /// # Parameters
    ///
    /// - `cap`: Number of 4096-byte buffers to allocate in the pool
    pub fn with_capacity(cap: usize) -> Self {
      let mut store = Vec::with_capacity(cap);

      for _ in 0..cap {
        store.push(Mutex::new(BufStoreBuffer::new()));
      }

      Self { store: store.into_boxed_slice() }
    }

    /// Tries to borrow a buffer from the pool.
    ///
    /// Returns `None` if all buffers are currently in use.
    /// The buffer is automatically returned to the pool when dropped.
    ///
    /// **Make sure to drop the [LentBuf]**.
    ///
    /// # Returns
    ///
    /// - `Some(LentBuf)`: A borrowed buffer from the pool
    /// - `None`: All buffers are in use
    pub fn try_get<'a>(&'a self) -> Option<LentBuf<'a>> {
      for i in 0..self.store.len() {
        if let Some(lock) = self.store[i].try_lock() {
          return Some(LentBuf(lock));
        }
      }

      return None;
    }
  }

  #[cfg(test)]
  mod tests {
    use super::*;
    use crate::buf::BufLike;

    #[test]
    fn nice() {
      fn assert_send<T: Send>() {}
      assert_send::<super::BufStore>();
    }

    #[test]
    fn test_lent_buf_into_iterator() {
      let store = BufStore::with_capacity(1);
      let mut buf = store.try_get().expect("should get buffer from pool");

      // Initialize and set some length
      buf.init();
      buf.0.len = 5;
      buf.0.buf[0] = b'A';
      buf.0.buf[1] = b'B';
      buf.0.buf[2] = b'C';
      buf.0.buf[3] = b'D';
      buf.0.buf[4] = b'E';

      // Test IntoIterator
      let collected: Vec<u8> = (&buf).into_iter().collect();
      assert_eq!(collected, vec![b'A', b'B', b'C', b'D', b'E']);
      assert_eq!(collected.len(), 5);

      // Test that we can iterate multiple times
      let count = (&buf).into_iter().count();
      assert_eq!(count, 5);
    }

    #[test]
    fn test_lent_buf_chunks() {
      let store = BufStore::with_capacity(1);
      let mut buf = store.try_get().expect("should get buffer from pool");

      buf.init();
      buf.0.len = 8;
      buf.0.buf[0] = b'A';
      buf.0.buf[1] = b'B';
      buf.0.buf[2] = b'C';
      buf.0.buf[3] = b'D';
      buf.0.buf[4] = b'E';
      buf.0.buf[5] = b'F';
      buf.0.buf[6] = b'G';
      buf.0.buf[7] = b'H';

      // Test chunks of size 3
      let chunks: Vec<&[u8]> = buf.chunks(3).collect();
      assert_eq!(chunks.len(), 3);
      assert_eq!(chunks[0], b"ABC");
      assert_eq!(chunks[1], b"DEF");
      assert_eq!(chunks[2], b"GH");
    }

    #[test]
    fn test_lent_buf_windows() {
      let store = BufStore::with_capacity(1);
      let mut buf = store.try_get().expect("should get buffer from pool");

      buf.init();
      buf.0.len = 5;
      buf.0.buf[0] = b'A';
      buf.0.buf[1] = b'B';
      buf.0.buf[2] = b'C';
      buf.0.buf[3] = b'D';
      buf.0.buf[4] = b'E';

      // Test windows of size 3
      let windows: Vec<&[u8]> = buf.windows(3).collect();
      assert_eq!(windows.len(), 3);
      assert_eq!(windows[0], b"ABC");
      assert_eq!(windows[1], b"BCD");
      assert_eq!(windows[2], b"CDE");
    }

    #[test]
    fn test_lent_buf_bytes_buf() {
      use bytes::Buf;

      let store = BufStore::with_capacity(1);
      let mut buf = store.try_get().expect("should get buffer from pool");

      buf.init();
      buf.0.len = 8;
      buf.0.buf[0] = b'H';
      buf.0.buf[1] = b'e';
      buf.0.buf[2] = b'l';
      buf.0.buf[3] = b'l';
      buf.0.buf[4] = b'o';
      buf.0.buf[5] = b'!';
      buf.0.buf[6] = b'!';
      buf.0.buf[7] = b'!';

      // Test remaining
      assert_eq!(buf.remaining(), 8);

      // Test chunk
      assert_eq!(buf.chunk(), b"Hello!!!");

      // Test get_u8
      assert_eq!(buf.get_u8(), b'H');
      assert_eq!(buf.remaining(), 7);
      assert_eq!(buf.chunk(), b"ello!!!");

      // Test advance
      buf.advance(4);
      assert_eq!(buf.remaining(), 3);
      assert_eq!(buf.chunk(), b"!!!");

      // Test copy_to_bytes
      let bytes = buf.copy_to_bytes(2);
      assert_eq!(&bytes[..], b"!!");
      assert_eq!(buf.remaining(), 1);
      assert_eq!(buf.chunk(), b"!");
    }
  }
}

#[cfg(test)]
mod tests {
  use crate::buf::Buf;

  #[test]
  fn buf() {
    assert_eq!(std::mem::size_of::<Buf<()>>(), 0);
  }
}
