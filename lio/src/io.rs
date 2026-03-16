//! I/O helper functions for common operations.
//!
//! This module provides helper functions for general I/O operations
//! that work across different resource types (files, sockets, pipes, etc.).
//!
//! For file-specific helpers like `read_to_string` and `write_all`,
//! see the methods on [`File`](crate::fs::File).
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

use crate::api::{self, resource::AsResource};

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
pub async fn copy(
  reader: &impl AsResource,
  writer: &impl AsResource,
) -> io::Result<u64> {
  let mut total: u64 = 0;

  loop {
    let buf = vec![0u8; DEFAULT_BUF_SIZE];
    let (read_result, buf) = api::read(reader, buf).await;
    let n = read_result? as usize;

    if n == 0 {
      break;
    }

    let to_write = buf[..n].to_vec();
    let mut written = 0;

    while written < n {
      let remaining = to_write[written..].to_vec();
      let (write_result, _) = api::write(writer, remaining).await;
      let w = write_result? as usize;

      if w == 0 {
        return Err(io::Error::new(
          io::ErrorKind::WriteZero,
          "write returned 0",
        ));
      }

      written += w;
    }

    total += n as u64;
  }

  Ok(total)
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
pub async fn copy_n(
  reader: &impl AsResource,
  writer: &impl AsResource,
  limit: u64,
) -> io::Result<u64> {
  let mut total: u64 = 0;

  while total < limit {
    let to_read = std::cmp::min(DEFAULT_BUF_SIZE as u64, limit - total) as usize;
    let buf = vec![0u8; to_read];
    let (read_result, buf) = api::read(reader, buf).await;
    let n = read_result? as usize;

    if n == 0 {
      break;
    }

    let to_write = buf[..n].to_vec();
    let mut written = 0;

    while written < n {
      let remaining = to_write[written..].to_vec();
      let (write_result, _) = api::write(writer, remaining).await;
      let w = write_result? as usize;

      if w == 0 {
        return Err(io::Error::new(
          io::ErrorKind::WriteZero,
          "write returned 0",
        ));
      }

      written += w;
    }

    total += n as u64;
  }

  Ok(total)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_default_buf_size() {
    assert_eq!(DEFAULT_BUF_SIZE, 64 * 1024);
  }
}
