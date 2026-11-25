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

use crate::api::{
  io::Io,
  ops,
  resource::{AsResource, Resource},
};
use crate::model::{Cx, OpModel, Step};
use crate::ready;
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
pub fn copy(reader: &impl AsResource, writer: &impl AsResource) -> Io<Copy> {
  Io::from_op(Copy::new_without_limit(
    reader.as_resource().clone(),
    writer.as_resource().clone(),
  ))
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
) -> Io<Copy> {
  Io::from_op(Copy::new_with_limit(
    reader.as_resource().clone(),
    writer.as_resource().clone(),
    limit,
  ))
}

pub struct Copy {
  reader: Resource,
  writer: Resource,
  total: u64,
  limit: u64,
  state: CopyState,
}

enum CopyState {
  Reading(ops::Read<CappedBuf>),
  Writing(ops::Write<Vec<u8>>),
}

impl Copy {
  fn new_without_limit(reader: Resource, writer: Resource) -> Self {
    Self::new_with_limit(reader, writer, u64::MAX)
  }
  fn new_with_limit(reader: Resource, writer: Resource, limit: u64) -> Self {
    let initial = limit.min(DEFAULT_BUF_SIZE as u64) as usize;
    let buf = CappedBuf::new(Vec::with_capacity(initial), initial);
    Self {
      state: CopyState::Reading(ops::Read::new(reader.clone(), buf, None)),
      reader,
      writer,
      total: 0,
      limit,
    }
  }

  fn write_zero() -> io::Error {
    io::Error::new(io::ErrorKind::WriteZero, "write returned 0")
  }

  fn next_read_len(&self) -> usize {
    (self.limit - self.total).min(DEFAULT_BUF_SIZE as u64) as usize
  }

  fn writing(&self, buf: Vec<u8>) -> CopyState {
    CopyState::Writing(ops::Write::new(self.writer.clone(), buf, None))
  }
}

impl OpModel for Copy {
  type Item = io::Result<u64>;

  fn poll(&mut self, cx: &mut Cx<'_>) -> Step<Self::Item> {
    loop {
      self.state = match &mut self.state {
        CopyState::Reading(read) => match ready!(read.poll(cx)) {
          (Ok(0), _buf) => return Step::Done(Ok(self.total)),
          (Ok(n), buf) => {
            self.total += n as u64;
            self.writing(buf.into_inner())
          }
          (Err(err), _buf) => return Step::Done(Err(err)),
        },
        CopyState::Writing(write) => match ready!(write.poll(cx)) {
          (Ok(0), _buf) => return Step::Done(Err(Self::write_zero())),
          (Ok(written), mut buf) if written < buf.len() => {
            let remaining = buf.len() - written;
            buf.copy_within(written.., 0);
            buf.truncate(remaining);
            self.writing(buf)
          }
          (Ok(_), _buf) if self.total >= self.limit => {
            return Step::Done(Ok(self.total));
          }
          (Ok(_), mut buf) => {
            buf.clear();
            let buf = CappedBuf::new(buf, self.next_read_len());
            CopyState::Reading(ops::Read::new(self.reader.clone(), buf, None))
          }
          (Err(err), _buf) => return Step::Done(Err(err)),
        },
      };
    }
  }
}

model_io!(Copy);

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
