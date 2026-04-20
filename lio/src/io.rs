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
//! use lio::{io, fs::File};
//!
//! async fn example() -> std::io::Result<()> {
//!     let src = File::open("/tmp/source.txt").await?;
//!     let dst = File::create("/tmp/dest.txt").await?;
//!     let bytes_copied = io::copy(&src, &dst).await?;
//!     println!("Copied {} bytes", bytes_copied);
//!     Ok(())
//! }
//! ```

use std::io;

use crate::api::{
  io::Io,
  op::{Action, Completion, OneshotOpModel, OpModel, OpResult},
  ops,
  resource::{AsResource, Resource},
};

/// Default buffer size for I/O operations (64 KB).
const DEFAULT_BUF_SIZE: usize = 64 * 1024;

/// Copies all data from reader to writer.
///
/// Returns the total number of bytes copied.
///
/// # Examples
///
/// ```rust,no_run
/// use lio::{io, fs::File};
///
/// async fn example() -> std::io::Result<()> {
///     let src = File::open("/tmp/source.txt").await?;
///     let dst = File::create("/tmp/dest.txt").await?;
///     let bytes = io::copy(&src, &dst).await?;
///     println!("Copied {} bytes", bytes);
///     Ok(())
/// }
/// ```
pub fn copy(reader: &impl AsResource, writer: &impl AsResource) -> Io<Copy> {
  Io::from_op(Copy::new(
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
/// use lio::{io, fs::File};
///
/// async fn example() -> std::io::Result<()> {
///     let src = File::open("/tmp/large_file.bin").await?;
///     let dst = File::create("/tmp/first_1mb.bin").await?;
///     let bytes = io::copy_n(&src, &dst, 1024 * 1024).await?;
///     println!("Copied {} bytes", bytes);
///     Ok(())
/// }
/// ```
pub fn copy_n(
  reader: &impl AsResource,
  writer: &impl AsResource,
  limit: u64,
) -> Io<CopyN> {
  Io::from_op(CopyN::new(
    reader.as_resource().clone(),
    writer.as_resource().clone(),
    limit,
  ))
}

pub struct Copy {
  inner: CopyN,
}

impl Copy {
  fn new(reader: Resource, writer: Resource) -> Self {
    Self { inner: CopyN::new(reader, writer, u64::MAX) }
  }
}

impl OpModel for Copy {
  type Item = io::Result<u64>;

  fn action(&mut self) -> Action {
    self.inner.action()
  }

  fn complete(&mut self, completion: Completion) -> OpResult<Self::Item> {
    self.inner.complete(completion)
  }
}

impl OneshotOpModel for Copy {}

pub struct CopyN {
  reader: Resource,
  writer: Resource,
  total: u64,
  limit: u64,
  state: CopyState,
}

enum CopyState {
  Reading(ops::Read<Vec<u8>>),
  Writing(ops::Write<Vec<u8>>),
  Done,
}

impl CopyN {
  fn new(reader: Resource, writer: Resource, limit: u64) -> Self {
    let initial = limit.min(DEFAULT_BUF_SIZE as u64) as usize;
    Self {
      reader: reader.clone(),
      writer: writer.clone(),
      total: 0,
      limit,
      state: CopyState::Reading(ops::Read::new(reader, vec![0u8; initial])),
    }
  }

  fn write_zero() -> io::Error {
    io::Error::new(io::ErrorKind::WriteZero, "write returned 0")
  }

  fn next_read_len(&self) -> usize {
    (self.limit - self.total).min(DEFAULT_BUF_SIZE as u64) as usize
  }
}

impl OpModel for CopyN {
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
        OpResult::Done((Ok(n), mut buf)) => {
          let n = n as usize;
          self.total += n as u64;
          buf.truncate(n);
          self.state =
            CopyState::Writing(ops::Write::new(self.writer.clone(), buf));
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
          let written = n as usize;
          if written < buf.len() {
            let remaining = buf.len() - written;
            buf.copy_within(written.., 0);
            buf.truncate(remaining);
            self.state =
              CopyState::Writing(ops::Write::new(self.writer.clone(), buf));
            return OpResult::Again;
          }

          if self.total >= self.limit {
            OpResult::Done(Ok(self.total))
          } else {
            buf.resize(self.next_read_len(), 0);
            self.state =
              CopyState::Reading(ops::Read::new(self.reader.clone(), buf));
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

impl OneshotOpModel for CopyN {}
