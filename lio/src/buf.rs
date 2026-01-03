//! Buffer abstractions for zero-copy I/O operations.
//!
//! This module provides buffer types and traits for efficient I/O without unnecessary
//! allocations. The core abstraction is [`BufLike`], which manages the lifecycle of
//! buffers used in syscalls.
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
//! # `zeroize`
//!
//! testing

/// Result type for operations that return both a result and a buffer.
///
/// This is commonly used for read/write operations where the buffer
/// is returned along with the operation result.
///
/// Example: See [`lio::read_with_buf`](crate::read_with_buf).
pub type BufResult<T, B> = (std::io::Result<T>, B);

trait Sealed {}

/// A interface over buffers.
pub trait BufLike: Sealed {
  fn buf<'a>(&'a self) -> &'a [u8];

  /// Called after I/O completes with the number of bytes transferred.
  ///
  /// For example this could be used for setting the "real" length of the buffer.
  ///
  /// # Parameters
  ///
  /// - `bytes`: bytes written (for writes) or bytes read (for reads)
  fn after(self, bytes: usize) -> Self;
}

impl BufLike for Vec<u8> {
  fn buf<'a>(&'a self) -> &'a [u8] {
    let cap = self.capacity();

    // Why capacity?
    // Because if consumer Vec::pop's, this lowers length, which means
    // that consumer would manually need to set length everytime.
    //
    // SAFETY: Safe as we use Vec::set_len in Self::after
    unsafe { slice::from_raw_parts(self.as_ptr(), cap) }
  }

  fn after(mut self, bw: usize) -> Self {
    // As we use capacity as buffer length, we need to set the valid
    // length of the vec.
    //
    // SAFETY: `bw` is from the kernel of how much it filled up the buffer.
    unsafe { self.set_len(bw) };
    self
  }
}

impl<B> Sealed for B where B: BufLike {}

use std::slice;

use std::array;
use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crossbeam_channel::{Receiver, Sender};
use std::time::Duration;

/// A borrowed buffer from a `BufStore` pool.
///
/// This buffer is automatically returned to the pool when dropped.
/// It implements `BufLike` and can be used for I/O operations without
/// allocating new memory.
///
/// **Make sure to drop**.
/// Doing any [`std::mem::forget`] will lead to bad things.
pub struct LentBuf<'a> {
  index: u32,
  pool: &'a BufStore,
}

// SAFETY: LentBuf just holds an index and reference to data.
// The actual buffer access is synchronized via AtomicBool in BufCell.
// Each LentBuf has exclusive access to its buffer (enforced by in_use flag).
unsafe impl<'a> Send for LentBuf<'a> {}
// SAFETY: ---- :: ----
unsafe impl<'a> Sync for LentBuf<'a> {}

impl<'a> super::BufLike for LentBuf<'a> {
  fn buf<'b>(&'b self) -> &'b [u8] {
    let cell = &self.pool.buffers[self.index as usize];
    // SAFETY: We have exclusive access via in_use flag
    unsafe { &*cell.buf.get() }
  }

  fn after(self, bw: usize) -> Self {
    let cell = &self.pool.buffers[self.index as usize];
    assert!(
      bw <= BUF_LEN,
      "LentBuf::after: bytes written ({}) exceeds buffer capacity ({})",
      bw,
      BUF_LEN
    );
    cell.len.store(bw, Ordering::Release);
    self
  }
}

impl<'a> AsRef<[u8]> for LentBuf<'a> {
  fn as_ref(&self) -> &[u8] {
    let cell = &self.pool.buffers[self.index as usize];
    let pos = cell.pos.load(Ordering::Acquire);
    let len = cell.len.load(Ordering::Acquire);
    // SAFETY: We have exclusive access via in_use flag
    &(unsafe { &(*cell.buf.get()) })[pos..len]
  }
}

impl<'a> Drop for LentBuf<'a> {
  fn drop(&mut self) {
    let cell = &self.pool.buffers[self.index as usize];

    #[cfg(feature = "zeroize")]
    {
      zeroize::Zeroize::zeroize(
        // SAFETY: We have exclusive access via in_use flag
        unsafe { &mut *cell.buf.get() },
      );
    }

    cell.len.store(0, Ordering::Relaxed);
    cell.pos.store(0, Ordering::Relaxed);

    // Mark buffer as available
    cell.in_use.store(false, Ordering::Release);

    // Return index to free list
    let _ = self.pool.free_tx.send(self.index);
  }
}

impl<'a, 'b> IntoIterator for &'a LentBuf<'b> {
  type Item = u8;
  type IntoIter = LentBufIter<'a, 'b>;

  fn into_iter(self) -> Self::IntoIter {
    LentBufIter { buf: self, index: 0 }
  }
}

pub struct LentBufIter<'a, 'b> {
  buf: &'a LentBuf<'b>,
  index: usize,
}

impl<'a, 'b> Iterator for LentBufIter<'a, 'b> {
  type Item = u8;
  fn next(&mut self) -> Option<Self::Item> {
    let cell = &self.buf.pool.buffers[self.buf.index as usize];
    let pos = cell.pos.load(Ordering::Acquire);
    let len = cell.len.load(Ordering::Acquire);
    let remaining = len - pos;
    if self.index >= remaining {
      None
    } else {
      let val = self.buf.as_ref()[self.index];
      self.index += 1;
      Some(val)
    }
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
#[cfg(feature = "bytes")]
impl<'a> bytes::Buf for LentBuf<'a> {
  fn remaining(&self) -> usize {
    let cell = &self.pool.buffers[self.index as usize];
    let pos = cell.pos.load(Ordering::Acquire);
    let len = cell.len.load(Ordering::Acquire);
    len - pos
  }

  fn chunk(&self) -> &[u8] {
    let cell = &self.pool.buffers[self.index as usize];
    let pos = cell.pos.load(Ordering::Acquire);
    let len = cell.len.load(Ordering::Acquire);
    // SAFETY: We have exclusive access via in_use flag
    &(unsafe { &(*cell.buf.get()) })[pos..len]
  }

  fn advance(&mut self, cnt: usize) {
    let cell = &self.pool.buffers[self.index as usize];
    let pos = cell.pos.load(Ordering::Acquire);
    let len = cell.len.load(Ordering::Acquire);
    let remaining = len - pos;
    assert!(
      cnt <= remaining,
      "LentBuf::advance: cannot advance by {} bytes, only {} remaining",
      cnt,
      remaining
    );

    cell.pos.store(pos + cnt, Ordering::Release);
  }
}

impl<'a> LentBuf<'a> {
  /// Returns an iterator over `chunk_size` chunks of the buffer.
  ///
  /// The last chunk may be shorter if the buffer length is not evenly divisible.
  ///
  /// # Panics
  ///
  /// Panics if `chunk_size` is 0.
  pub fn chunks(&self, chunk_size: usize) -> std::slice::Chunks<'_, u8> {
    self.as_ref().chunks(chunk_size)
  }

  /// Returns an iterator over overlapping windows of `window_size`.
  ///
  /// Each window overlaps with the previous by `window_size - 1` bytes.
  ///
  /// # Panics
  ///
  /// Panics if `window_size` is 0 or greater than buffer length.
  pub fn windows(&self, window_size: usize) -> std::slice::Windows<'_, u8> {
    self.as_ref().windows(window_size)
  }
}

const BUF_LEN: usize = 4096;

/// A single buffer cell in the pool.
///
/// Contains the actual buffer data and atomic metadata for thread-safe access.
struct BufCell {
  buf: UnsafeCell<[u8; BUF_LEN]>,
  len: AtomicUsize,
  pos: AtomicUsize,
  in_use: AtomicBool,
}

// SAFETY: BufCell is Sync because access to the UnsafeCell is synchronized
// via the atomic in_use flag. Only one thread can access the buffer at a time
// (enforced by the CAS in try_get).
unsafe impl Sync for BufCell {}

impl BufCell {
  fn new() -> Self {
    Self {
      buf: UnsafeCell::new(array::from_fn::<_, BUF_LEN, _>(|_| 0)),
      len: AtomicUsize::new(0),
      pos: AtomicUsize::new(0),
      in_use: AtomicBool::new(false),
    }
  }
}

/// A pool of reusable buffers for I/O operations.
///
/// Provides zero-allocation buffer lending using an index-based design.
/// All buffers are allocated upfront, and lending/returning uses lock-free operations.
///
/// # Design
///
/// - All buffers allocated once at initialization (zero runtime allocations)
/// - Index-based lending (LentBuf holds index, not mutex guard)
/// - Lock-free free list using crossbeam SegQueue
/// - Atomic flags for exclusive access (no mutexes per buffer)
///
/// # Example
///
/// ```ignore
/// let buf_store = BufStore::with_capacity(64); // 64 * 4KB allocated once
///
/// if let Some(buf) = buf_store.try_get() {
///     // Got a buffer from pool (zero allocations)
/// } else {
///     // All buffers in use
/// }
/// ```
pub struct BufStore {
  buffers: Box<[BufCell]>,
  free_tx: Sender<u32>,
  free_rx: Receiver<u32>,
}

impl Default for BufStore {
  fn default() -> Self {
    Self::with_capacity(128)
  }
}

impl BufStore {
  /// Creates a new buffer pool with the specified capacity.
  ///
  /// Allocates all buffers upfront. After initialization, try_get() performs
  /// zero heap allocations.
  ///
  /// # Parameters
  ///
  /// - `cap`: Number of 4096-byte buffers to allocate in the pool
  pub fn with_capacity(cap: usize) -> Self {
    let mut buffers = Vec::with_capacity(cap);
    let (free_tx, free_rx) = crossbeam_channel::unbounded();

    for i in 0..cap {
      buffers.push(BufCell::new());
      // Pre-populate the channel with all buffer indices
      free_tx.send(i as u32).expect("channel should not be full");
    }

    Self { buffers: buffers.into_boxed_slice(), free_tx, free_rx }
  }

  /// Tries to borrow a buffer from the pool.
  ///
  /// Returns `None` if all buffers are currently in use.
  /// The buffer is automatically returned to the pool when dropped.
  ///
  /// **Zero heap allocations** - just pops an index and returns a small stack struct.
  ///
  /// **Make sure to drop the [LentBuf]**.
  ///
  /// # Returns
  ///
  /// - `Some(LentBuf)`: A borrowed buffer from the pool
  /// - `None`: All buffers are in use
  pub fn try_get(&self) -> Option<LentBuf<'_>> {
    let index = self.free_rx.try_recv().ok()?;
    Some(self.acquire_buffer(index))
  }

  /// Blocks until a buffer is available and returns it.
  ///
  /// This will wait indefinitely until a buffer becomes available.
  /// The buffer is automatically returned to the pool when dropped.
  ///
  /// # Returns
  ///
  /// A borrowed buffer from the pool
  pub fn get(&self) -> LentBuf<'_> {
    let index = self.free_rx.recv().expect("channel should not disconnect");
    self.acquire_buffer(index)
  }

  /// Tries to borrow a buffer with a timeout.
  ///
  /// Returns `None` if no buffer becomes available within the timeout period.
  ///
  /// # Arguments
  ///
  /// * `timeout` - Maximum time to wait for a buffer
  ///
  /// # Returns
  ///
  /// - `Some(LentBuf)`: A borrowed buffer from the pool
  /// - `None`: Timeout expired before a buffer became available
  pub fn get_timeout(&self, timeout: Duration) -> Option<LentBuf<'_>> {
    match self.free_rx.recv_timeout(timeout) {
      Ok(index) => Some(self.acquire_buffer(index)),
      Err(_) => None,
    }
  }

  /// Internal helper to acquire a buffer by index.
  fn acquire_buffer(&self, index: u32) -> LentBuf<'_> {
    let cell = &self.buffers[index as usize];
    let was_in_use = cell.in_use.swap(true, Ordering::Acquire);

    assert!(
      !was_in_use,
      "BufStore invariant violated: buffer {} (popped from free_list) already in use",
      index
    );

    // Reset state for new use
    cell.len.store(0, Ordering::Relaxed);
    cell.pos.store(0, Ordering::Relaxed);

    LentBuf { index, pool: self }
  }

  /// Returns the total capacity of the buffer pool.
  pub fn capacity(&self) -> usize {
    self.buffers.len()
  }

  /// Returns the number of currently available buffers.
  ///
  /// Note: This is a snapshot and may be stale immediately.
  pub fn available(&self) -> usize {
    self.free_rx.len()
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn make_sure_send_and_sync() {
    fn assert_send<T: Send>() {}
    fn assert_sync<T: Sync>() {}
    assert_send::<super::BufStore>();
    assert_send::<super::LentBuf>();
    assert_sync::<super::LentBuf>();
  }

  #[test]
  fn test_lent_buf_into_iterator() {
    // Need static lifetime for new design
    let store = Box::leak(Box::new(BufStore::with_capacity(1)));
    let buf = store.try_get().expect("should get buffer from pool");

    // Set up buffer data
    let cell = &store.buffers[buf.index as usize];
    unsafe {
      let buf_ptr = cell.buf.get();
      (*buf_ptr)[0] = b'A';
      (*buf_ptr)[1] = b'B';
      (*buf_ptr)[2] = b'C';
      (*buf_ptr)[3] = b'D';
      (*buf_ptr)[4] = b'E';
    }
    cell.len.store(5, Ordering::Release);

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
    let store = Box::leak(Box::new(BufStore::with_capacity(1)));
    let buf = store.try_get().expect("should get buffer from pool");

    let cell = &store.buffers[buf.index as usize];
    unsafe {
      let buf_ptr = cell.buf.get();
      (*buf_ptr)[0] = b'A';
      (*buf_ptr)[1] = b'B';
      (*buf_ptr)[2] = b'C';
      (*buf_ptr)[3] = b'D';
      (*buf_ptr)[4] = b'E';
      (*buf_ptr)[5] = b'F';
      (*buf_ptr)[6] = b'G';
      (*buf_ptr)[7] = b'H';
    }
    cell.len.store(8, Ordering::Release);

    // Test chunks of size 3
    let chunks: Vec<&[u8]> = buf.chunks(3).collect();
    assert_eq!(chunks.len(), 3);
    assert_eq!(chunks[0], b"ABC");
    assert_eq!(chunks[1], b"DEF");
    assert_eq!(chunks[2], b"GH");
  }

  #[test]
  fn test_lent_buf_windows() {
    let store = Box::leak(Box::new(BufStore::with_capacity(1)));
    let buf = store.try_get().expect("should get buffer from pool");

    let cell = &store.buffers[buf.index as usize];
    unsafe {
      let buf_ptr = cell.buf.get();
      (*buf_ptr)[0] = b'A';
      (*buf_ptr)[1] = b'B';
      (*buf_ptr)[2] = b'C';
      (*buf_ptr)[3] = b'D';
      (*buf_ptr)[4] = b'E';
    }
    cell.len.store(5, Ordering::Release);

    // Test windows of size 3
    let windows: Vec<&[u8]> = buf.windows(3).collect();
    assert_eq!(windows.len(), 3);
    assert_eq!(windows[0], b"ABC");
    assert_eq!(windows[1], b"BCD");
    assert_eq!(windows[2], b"CDE");
  }

  #[test]
  #[cfg(feature = "bytes")]
  fn test_lent_buf_bytes_buf() {
    use bytes::Buf;

    let store = Box::leak(Box::new(BufStore::with_capacity(1)));
    let mut buf = store.try_get().expect("should get buffer from pool");

    let cell = &store.buffers[buf.index as usize];
    unsafe {
      let buf_ptr = cell.buf.get();
      (*buf_ptr)[0] = b'H';
      (*buf_ptr)[1] = b'e';
      (*buf_ptr)[2] = b'l';
      (*buf_ptr)[3] = b'l';
      (*buf_ptr)[4] = b'o';
      (*buf_ptr)[5] = b'!';
      (*buf_ptr)[6] = b'!';
      (*buf_ptr)[7] = b'!';
    }
    cell.len.store(8, Ordering::Release);

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

  // ============================================================================
  // BufStore Tests
  // ============================================================================

  #[test]
  fn test_bufstore_basic_allocation() {
    let store = Box::leak(Box::new(BufStore::with_capacity(4)));

    // Should be able to allocate all buffers
    let buf1 = store.try_get().expect("should get buffer 1");
    let buf2 = store.try_get().expect("should get buffer 2");
    let buf3 = store.try_get().expect("should get buffer 3");
    let buf4 = store.try_get().expect("should get buffer 4");

    // All buffers allocated, next should fail
    assert!(store.try_get().is_none(), "should not get buffer when all in use");

    // Drop one buffer
    drop(buf2);

    // Should be able to get one more
    let buf5 = store.try_get().expect("should get buffer after one dropped");

    // Still full
    assert!(store.try_get().is_none());

    // Clean up
    drop(buf1);
    drop(buf3);
    drop(buf4);
    drop(buf5);
  }

  #[test]
  fn test_bufstore_buffer_reuse() {
    let store = Box::leak(Box::new(BufStore::with_capacity(2)));

    // Get and drop a buffer multiple times
    for i in 0..10 {
      let buf = store.try_get().expect("should get buffer");

      // Write some data
      let cell = &store.buffers[buf.index as usize];
      unsafe {
        (*cell.buf.get())[0] = i;
      }
      cell.len.store(1, Ordering::Release);

      drop(buf);

      // Immediately get it again
      let buf2 = store.try_get().expect("should get buffer after drop");

      // Buffer should be reset (len = 0)
      let cell2 = &store.buffers[buf2.index as usize];
      assert_eq!(
        cell2.len.load(Ordering::Acquire),
        0,
        "buffer should be reset"
      );
      assert_eq!(cell2.pos.load(Ordering::Acquire), 0, "pos should be reset");

      drop(buf2);
    }
  }

  #[test]
  fn test_bufstore_unique_indices() {
    let store = Box::leak(Box::new(BufStore::with_capacity(8)));

    // Get all buffers
    let bufs: Vec<_> = (0..8).map(|_| store.try_get().unwrap()).collect();

    // Extract indices
    let indices: Vec<u32> = bufs.iter().map(|b| b.index).collect();

    // All indices should be unique
    for i in 0..indices.len() {
      for j in (i + 1)..indices.len() {
        assert_ne!(
          indices[i], indices[j],
          "indices should be unique: index {} == {} at positions {} and {}",
          indices[i], indices[j], i, j
        );
      }
    }

    // All indices should be in valid range
    for &index in &indices {
      assert!((index as usize) < 8, "index {} should be < capacity 8", index);
    }
  }

  #[test]
  fn test_bufstore_single_capacity() {
    let store = Box::leak(Box::new(BufStore::with_capacity(1)));

    // Should get one buffer
    let buf1 = store.try_get();
    assert!(buf1.is_some());

    // Second should fail
    assert!(store.try_get().is_none());

    // Drop and get again
    drop(buf1);
    assert!(store.try_get().is_some());
  }

  #[test]
  fn test_bufstore_drop_order() {
    let store = Box::leak(Box::new(BufStore::with_capacity(3)));

    let buf1 = store.try_get().unwrap();
    let buf2 = store.try_get().unwrap();
    let buf3 = store.try_get().unwrap();

    // Record indices
    let idx1 = buf1.index;
    let idx2 = buf2.index;
    let idx3 = buf3.index;

    // Drop in different order
    drop(buf2);
    drop(buf1);
    drop(buf3);

    // Get them back - order might be different (stack-like from SegQueue)
    let new1 = store.try_get().unwrap();
    let new2 = store.try_get().unwrap();
    let new3 = store.try_get().unwrap();

    let new_indices = vec![new1.index, new2.index, new3.index];
    let old_indices = vec![idx1, idx2, idx3];

    // Should have same set of indices (possibly different order)
    for &old_idx in &old_indices {
      assert!(
        new_indices.contains(&old_idx),
        "new indices {:?} should contain old index {}",
        new_indices,
        old_idx
      );
    }
  }

  // ============================================================================
  // Stress Tests
  // ============================================================================

  #[test]
  fn test_bufstore_stress_sequential() {
    let store = Box::leak(Box::new(BufStore::with_capacity(16)));

    // Allocate and free buffers many times
    for _ in 0..1000 {
      let bufs: Vec<_> = (0..16).map(|_| store.try_get().unwrap()).collect();
      drop(bufs);
    }
  }

  #[test]
  fn test_bufstore_stress_partial_usage() {
    let store = Box::leak(Box::new(BufStore::with_capacity(32)));

    for _ in 0..100 {
      // Allocate random number of buffers
      let count = (fastrand::usize(..)) % 32 + 1;
      let bufs: Vec<_> = (0..count).filter_map(|_| store.try_get()).collect();

      assert!(bufs.len() <= count);

      // Drop half of them
      let mut bufs = bufs;
      bufs.truncate(bufs.len() / 2);
      drop(bufs);
    }
  }

  #[test]
  #[cfg(not(miri))] // Skip under miri - too slow
  fn test_bufstore_stress_concurrent() {
    use std::sync::Arc;
    use std::thread;

    let store: &'static BufStore =
      Box::leak(Box::new(BufStore::with_capacity(64)));
    let iterations = 10000;
    let num_threads = 8;

    let handles: Vec<_> = (0..num_threads)
      .map(|_| {
        thread::spawn(move || {
          for _ in 0..iterations {
            if let Some(buf) = store.try_get() {
              // Simulate some work
              let cell = &store.buffers[buf.index as usize];
              unsafe {
                (*cell.buf.get())[0] = 42;
              }
              cell.len.store(1, Ordering::Release);

              // Drop buffer
              drop(buf);
            }
          }
        })
      })
      .collect();

    for handle in handles {
      handle.join().unwrap();
    }

    // At the end, all buffers should be back in the pool
    let recovered: Vec<_> = (0..64).filter_map(|_| store.try_get()).collect();

    assert_eq!(
      recovered.len(),
      64,
      "all buffers should be returned to pool after concurrent stress test"
    );
  }

  #[test]
  #[cfg(not(miri))]
  fn test_bufstore_stress_concurrent_heavy_contention() {
    use std::thread;
    use std::time::Duration;

    // Small pool, many threads = high contention
    let store: &'static BufStore =
      Box::leak(Box::new(BufStore::with_capacity(4)));
    let num_threads = 16;
    let duration = Duration::from_millis(100);

    let handles: Vec<_> = (0..num_threads)
      .map(|_| {
        thread::spawn(move || {
          let start = std::time::Instant::now();
          let mut acquired_count = 0;

          while start.elapsed() < duration {
            if let Some(buf) = store.try_get() {
              acquired_count += 1;

              // Hold buffer briefly
              let cell = &store.buffers[buf.index as usize];
              cell.len.store(42, Ordering::Release);

              // Small delay to increase contention
              std::hint::spin_loop();

              drop(buf);
            }
          }

          acquired_count
        })
      })
      .collect();

    let total_acquisitions: usize =
      handles.into_iter().map(|h| h.join().unwrap()).sum();

    println!(
      "Total buffer acquisitions under heavy contention: {}",
      total_acquisitions
    );
    assert!(total_acquisitions > 0, "should have acquired some buffers");

    // Verify all buffers returned
    let recovered: Vec<_> = (0..4).filter_map(|_| store.try_get()).collect();
    assert_eq!(recovered.len(), 4, "all buffers should be back in pool");
  }

  #[test]
  #[cfg(not(miri))]
  fn test_bufstore_no_lost_buffers() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use std::thread;

    let store: &'static BufStore =
      Box::leak(Box::new(BufStore::with_capacity(32)));
    let acquired = Arc::new(AtomicUsize::new(0));
    let released = Arc::new(AtomicUsize::new(0));

    let handles: Vec<_> = (0..8)
      .map(|_| {
        let acquired = Arc::clone(&acquired);
        let released = Arc::clone(&released);

        thread::spawn(move || {
          for _ in 0..1000 {
            if let Some(buf) = store.try_get() {
              acquired.fetch_add(1, AtomicOrdering::Relaxed);

              // Simulate work
              std::thread::yield_now();

              drop(buf);
              released.fetch_add(1, AtomicOrdering::Relaxed);
            }
          }
        })
      })
      .collect();

    for handle in handles {
      handle.join().unwrap();
    }

    // All acquired buffers should be released
    assert_eq!(
      acquired.load(AtomicOrdering::Relaxed),
      released.load(AtomicOrdering::Relaxed),
      "acquired and released counts should match"
    );

    // All buffers should be available
    let available: Vec<_> = (0..32).filter_map(|_| store.try_get()).collect();
    assert_eq!(available.len(), 32, "no buffers should be lost");
  }
}
