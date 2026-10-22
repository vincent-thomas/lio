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
  op::{Action, Completion, OneshotOpModel, OpModel, OpResult},
  ops,
  resource::{AsResource, Resource},
};
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

impl<B: IoBufVec> IoBufVec for WriteCursor<B> {
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
  Done,
}

impl Copy {
  fn new_without_limit(reader: Resource, writer: Resource) -> Self {
    Self::new_with_limit(reader, writer, u64::MAX)
  }
  fn new_with_limit(reader: Resource, writer: Resource, limit: u64) -> Self {
    let initial = limit.min(DEFAULT_BUF_SIZE as u64) as usize;
    let buf = CappedBuf::new(Vec::with_capacity(initial), initial);
    Self {
      reader: reader.clone(),
      writer: writer.clone(),
      total: 0,
      limit,
      state: CopyState::Reading(ops::Read::new(reader, buf, None)),
    }
  }

  fn write_zero() -> io::Error {
    io::Error::new(io::ErrorKind::WriteZero, "write returned 0")
  }

  fn next_read_len(&self) -> usize {
    (self.limit - self.total).min(DEFAULT_BUF_SIZE as u64) as usize
  }
}

// SAFETY: every `Op` comes from the `action` of an inner `OpModel`, which
// stays in place in `self` until its completion is forwarded to it, or carries
// no pointers.
unsafe impl OpModel for Copy {
  type Item = io::Result<u64>;

  fn action(&mut self) -> Action {
    match &mut self.state {
      CopyState::Reading(read) => read.action(),
      CopyState::Writing(write) => write.action(),
      CopyState::Done => panic!("CopyN polled after completion"),
    }
  }

  fn complete(&mut self, completion: Completion) -> OpResult<Self::Item> {
    match std::mem::replace(&mut self.state, CopyState::Done) {
      CopyState::Reading(mut read) => match read.complete(completion) {
        OpResult::Done((Ok(0), _buf)) => OpResult::Done(Ok(self.total)),
        OpResult::Done((Ok(n), buf)) => {
          self.total += n as u64;
          let buf = buf.into_inner();
          self.state =
            CopyState::Writing(ops::Write::new(self.writer.clone(), buf, None));
          OpResult::Again
        }
        OpResult::Done((Err(err), _buf)) => OpResult::Done(Err(err)),
        OpResult::Again | OpResult::Yield(_) => unreachable!(),
      },
      CopyState::Writing(mut write) => match write.complete(completion) {
        OpResult::Done((Ok(0), _buf)) => {
          OpResult::Done(Err(Self::write_zero()))
        }
        OpResult::Done((Ok(n), mut buf)) => {
          let written = n;
          if written < buf.len() {
            let remaining = buf.len() - written;
            buf.copy_within(written.., 0);
            buf.truncate(remaining);
            self.state = CopyState::Writing(ops::Write::new(
              self.writer.clone(),
              buf,
              None,
            ));
            return OpResult::Again;
          }

          if self.total >= self.limit {
            OpResult::Done(Ok(self.total))
          } else {
            buf.clear();
            let buf = CappedBuf::new(buf, self.next_read_len());
            self.state = CopyState::Reading(ops::Read::new(
              self.reader.clone(),
              buf,
              None,
            ));
            OpResult::Again
          }
        }
        OpResult::Done((Err(err), _buf)) => OpResult::Done(Err(err)),
        OpResult::Again | OpResult::Yield(_) => unreachable!(),
      },
      CopyState::Done => panic!("CopyN completed after terminal state"),
    }
  }
}

impl OneshotOpModel for Copy {}

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

impl IoBuf for CappedBuf {
  fn as_slice(&self) -> &[u8] {
    &self.buf
  }
}

impl IoBufMut for CappedBuf {
  fn as_uninit_mut(&mut self) -> &mut [MaybeUninit<u8>] {
    let cap = self.cap;
    &mut self.buf.as_uninit_mut()[..cap]
  }

  unsafe fn set_len(&mut self, len: usize) {
    // SAFETY: `len <= self.cap <= capacity`, and the caller guarantees the
    // first `len` bytes are initialized.
    unsafe { IoBufMut::set_len(&mut self.buf, len) }
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
