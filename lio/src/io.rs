//! I/O helper functions for common operations.
//!
//! This module provides helper functions for general I/O operations
//! that work across different resource types (files, sockets, pipes, etc.).
//!
//! For file-specific helpers like `read_to_string` and `write_all`,
//! see the file APIs exposed by lio's higher-level resource types.
//!
//! # Examples
//!
//! ## Copy between resources
//!
//! ```rust,no_run
//! use lio::{io, api::resource::Resource};
//!
//! async fn example() -> std::io::Result<()> {
//!     let src = Resource::stdin();
//!     let dst = Resource::stdout();
//!     let bytes_copied = io::copy(&src, &dst).await?;
//!     println!("Copied {} bytes", bytes_copied);
//!     Ok(())
//! }
//! ```

use std::io;
use std::mem::MaybeUninit;
use std::task::Poll;

use crate::api::{
  ops,
  resource::{AsResource, Resource},
};
use crate::model::{Cx, OpModel};
use crate::{IoBuf, IoBufMut, IoBufVec};

/// Default buffer size for I/O operations (64 KB).
const DEFAULT_BUF_SIZE: usize = 64 * 1024;

pub struct WriteCursor<B> {
  inner: B,
  chunk_idx: usize,
  chunk_offset: usize,
}

impl<B> WriteCursor<B> {
  pub fn new(inner: B) -> Self {
    Self { inner, chunk_idx: 0, chunk_offset: 0 }
  }
}

impl<B: IoBufVec> WriteCursor<B> {
  pub fn is_empty(&self) -> bool {
    self.chunk_idx >= self.inner.buf_count()
  }

  pub fn advance(&mut self, mut written: usize) {
    while written > 0 && self.chunk_idx < self.inner.buf_count() {
      let len = self.inner.buf(self.chunk_idx).len();
      let remaining = len.saturating_sub(self.chunk_offset);
      if written < remaining {
        self.chunk_offset += written;
        return;
      }
      written -= remaining;
      self.chunk_idx += 1;
      self.chunk_offset = 0;
    }
  }
}

// SAFETY: every slice is a subslice of `inner`, whose `IoBufVec` promise
// keeps those bytes in place.
unsafe impl<B: IoBufVec> IoBufVec for WriteCursor<B> {
  fn buf_count(&self) -> usize {
    self.inner.buf_count().saturating_sub(self.chunk_idx)
  }

  fn buf(&self, i: usize) -> &[u8] {
    let buf = self.inner.buf(self.chunk_idx + i);
    let offset = if i == 0 { self.chunk_offset } else { 0 };
    buf.get(offset..).unwrap_or_default()
  }
}

/// Copies all data from reader to writer.
///
/// Returns the total number of bytes copied.
///
/// # Examples
///
/// ```rust,no_run
/// use lio::{io, api::resource::Resource};
///
/// async fn example() -> std::io::Result<()> {
///     let src = Resource::stdin();
///     let dst = Resource::stdout();
///     let bytes = io::copy(&src, &dst).await?;
///     println!("Copied {} bytes", bytes);
///     Ok(())
/// }
/// ```
pub fn copy(reader: &impl AsResource, writer: &impl AsResource) -> Copy {
  Copy::new_without_limit(
    reader.as_resource().clone(),
    writer.as_resource().clone(),
  )
}

/// Copies up to `limit` bytes from reader to writer.
///
/// Returns the total number of bytes copied (may be less than `limit` if EOF reached).
///
/// # Examples
///
/// ```rust,no_run
/// use lio::{io, api::resource::Resource};
///
/// async fn example() -> std::io::Result<()> {
///     let src = Resource::stdin();
///     let dst = Resource::stdout();
///     let bytes = io::copy_n(&src, &dst, 1024 * 1024).await?;
///     println!("Copied {} bytes", bytes);
///     Ok(())
/// }
/// ```
pub fn copy_n(
  reader: &impl AsResource,
  writer: &impl AsResource,
  limit: u64,
) -> Copy {
  Copy::new_with_limit(
    reader.as_resource().clone(),
    writer.as_resource().clone(),
    limit,
  )
}

/// Copies from one resource to another, reading the next chunk while the
/// one before it is being written.
///
/// At most one read and one write are in flight, so bytes arrive in order.
/// A chunk that has been read waits until the write before it is done, and
/// no further read starts while one is waiting: the copy holds two buffers.
#[must_use = "an operation does nothing until it is run or polled"]
pub struct Copy {
  reader: Resource,
  writer: Resource,
  /// Bytes read so far. Never past `limit`.
  read: u64,
  limit: u64,
  /// The reader reported its end.
  eof: bool,
  reading: Option<ops::Read<CappedBuf>>,
  writing: Option<ops::Write<Vec<u8>>>,
  /// A chunk read while the one before it was still being written.
  ready: Option<Vec<u8>>,
  /// An empty buffer for the next read.
  spare: Option<Vec<u8>>,
}

impl Copy {
  fn new_without_limit(reader: Resource, writer: Resource) -> Self {
    Self::new_with_limit(reader, writer, u64::MAX)
  }

  fn new_with_limit(reader: Resource, writer: Resource, limit: u64) -> Self {
    Self {
      reader,
      writer,
      read: 0,
      limit,
      eof: false,
      reading: None,
      writing: None,
      ready: None,
      spare: None,
    }
  }

  fn write_zero() -> io::Error {
    io::Error::new(io::ErrorKind::WriteZero, "write returned 0")
  }

  /// A read of the next chunk, no longer than what is left of the limit.
  fn next_read(&mut self) -> ops::Read<CappedBuf> {
    let len = (self.limit - self.read).min(DEFAULT_BUF_SIZE as u64) as usize;
    let buf = match self.spare.take() {
      Some(buf) => buf,
      None => Vec::with_capacity(len),
    };
    ops::Read::new(self.reader.clone(), CappedBuf::new(buf, len), None)
  }

  fn write(&self, chunk: Vec<u8>) -> ops::Write<Vec<u8>> {
    ops::Write::new(self.writer.clone(), chunk, None)
  }
}

runnable!([] Copy);

impl OpModel for Copy {
  type Item = io::Result<u64>;

  fn poll(&mut self, cx: &mut Cx<'_>) -> Poll<Self::Item> {
    loop {
      // Something finished or started: go around again, so that what
      // started is submitted and what it unblocked can start.
      let mut progressed = false;

      // The write goes first: on a driver with one free slot, what was
      // read is written before more is read.
      if let Some(write) = &mut self.writing
        && let Poll::Ready(done) = write.poll(cx)
      {
        self.writing = None;
        progressed = true;
        match done {
          (Ok(0), _chunk) => return Poll::Ready(Err(Self::write_zero())),
          (Ok(written), mut chunk) if written < chunk.len() => {
            let remaining = chunk.len() - written;
            chunk.copy_within(written.., 0);
            chunk.truncate(remaining);
            self.writing = Some(self.write(chunk));
          }
          (Ok(_), mut chunk) => {
            chunk.clear();
            // The other buffer is with the reader, or waiting in `ready`.
            self.spare.get_or_insert(chunk);
          }
          (Err(err), _chunk) => return Poll::Ready(Err(err)),
        }
      }

      if let Some(read) = &mut self.reading
        && let Poll::Ready(done) = read.poll(cx)
      {
        self.reading = None;
        progressed = true;
        match done {
          (Ok(0), buf) => {
            self.eof = true;
            self.spare = Some(buf.into_inner());
          }
          (Ok(n), buf) => {
            self.read += n as u64;
            self.ready = Some(buf.into_inner());
          }
          (Err(err), _buf) => return Poll::Ready(Err(err)),
        }
      }

      if self.writing.is_none()
        && let Some(chunk) = self.ready.take()
      {
        self.writing = Some(self.write(chunk));
        progressed = true;
      }

      let more = !self.eof && self.read < self.limit;
      if more && self.reading.is_none() && self.ready.is_none() {
        self.reading = Some(self.next_read());
        progressed = true;
      }

      if self.reading.is_none() && self.writing.is_none() && !more {
        return Poll::Ready(Ok(self.read));
      }
      if !progressed {
        return Poll::Pending;
      }
    }
  }
}

/// A read buffer whose writable region is capped below the `Vec`'s capacity.
///
/// A `Vec<u8>` lends reads its whole capacity, so reusing one allocation for
/// a shorter read would let the read overrun the remaining limit.
struct CappedBuf {
  buf: Vec<u8>,
  cap: usize,
}

impl CappedBuf {
  fn new(buf: Vec<u8>, cap: usize) -> Self {
    let cap = cap.min(buf.capacity());
    Self { buf, cap }
  }

  fn into_inner(self) -> Vec<u8> {
    self.buf
  }
}

// SAFETY: `CappedBuf` borrows a `Vec<u8>`, which does not move its allocation
// under a shared borrow.
unsafe impl IoBuf for CappedBuf {
  fn as_slice(&self) -> &[u8] {
    &self.buf
  }
}

// SAFETY: the region is the `Vec` allocation, capped. `commit` uses
// `Initialized` for that allocation.
unsafe impl IoBufMut for CappedBuf {
  fn as_uninit_mut(&mut self) -> &mut [MaybeUninit<u8>] {
    let cap = self.cap;
    &mut self.buf.as_uninit_mut()[..cap]
  }

  fn commit(&mut self, initialized: crate::Initialized<'_>) {
    assert!(
      initialized.len() <= self.cap,
      "commit length exceeds the capped buffer"
    );
    initialized.apply_to_vec(&mut self.buf);
  }
}

#[cfg(test)]
mod tests {
  use super::WriteCursor;
  use crate::IoBufVec;

  fn buf_bytes<B: IoBufVec>(bufs: &B, idx: usize) -> Vec<u8> {
    bufs.buf(idx).to_vec()
  }

  #[test]
  fn write_cursor_starts_at_first_chunk() {
    let cursor =
      WriteCursor::new((b"abc".to_vec(), b"de".to_vec(), b"f".to_vec()));

    assert!(!cursor.is_empty());
    assert_eq!(cursor.buf_count(), 3);
    assert_eq!(buf_bytes(&cursor, 0), b"abc");
    assert_eq!(buf_bytes(&cursor, 1), b"de");
    assert_eq!(buf_bytes(&cursor, 2), b"f");
  }

  #[test]
  fn write_cursor_advances_within_current_chunk() {
    let mut cursor =
      WriteCursor::new((b"abc".to_vec(), b"de".to_vec(), b"f".to_vec()));

    cursor.advance(2);

    assert!(!cursor.is_empty());
    assert_eq!(cursor.buf_count(), 3);
    assert_eq!(buf_bytes(&cursor, 0), b"c");
    assert_eq!(buf_bytes(&cursor, 1), b"de");
    assert_eq!(buf_bytes(&cursor, 2), b"f");
  }

  #[test]
  fn write_cursor_advances_across_chunk_boundaries() {
    let mut cursor =
      WriteCursor::new((b"abc".to_vec(), b"de".to_vec(), b"fghi".to_vec()));

    cursor.advance(4);

    assert!(!cursor.is_empty());
    assert_eq!(cursor.buf_count(), 2);
    assert_eq!(buf_bytes(&cursor, 0), b"e");
    assert_eq!(buf_bytes(&cursor, 1), b"fghi");
  }

  #[test]
  fn write_cursor_becomes_empty_after_exact_consumption() {
    let mut cursor =
      WriteCursor::new((b"abc".to_vec(), b"de".to_vec(), b"fghi".to_vec()));

    cursor.advance(9);

    assert!(cursor.is_empty());
    assert_eq!(cursor.buf_count(), 0);
  }
}
