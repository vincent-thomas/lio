//! Filesystem operations for lio.
//!
//! This module provides high-level abstractions for filesystem I/O operations,
//! including [`File`] for reading/writing files and [`OpenOptions`] for
//! configuring how files are opened.
//!
//! # Examples
//!
//! ## Reading a file
//!
//! ```rust,no_run
//! use lio::fs;
//!
//! async fn read_example() -> std::io::Result<()> {
//!     let contents = fs::read_to_string("/tmp/example.txt").await?;
//!     println!("Contents: {}", contents);
//!     Ok(())
//! }
//! ```
//!
//! ## Writing a file
//!
//! ```rust,no_run
//! use lio::fs;
//!
//! async fn write_example() -> std::io::Result<()> {
//!     fs::write("/tmp/output.txt", b"Hello, World!").await?;
//!     Ok(())
//! }
//! ```
//!
//! ## Using OpenOptions
//!
//! ```rust,no_run
//! use lio::fs::OpenOptions;
//!
//! async fn open_options_example() -> std::io::Result<()> {
//!     let file = OpenOptions::new()
//!         .read(true)
//!         .write(true)
//!         .create(true)
//!         .open("/tmp/readwrite.txt")
//!         .await?;
//!     Ok(())
//! }
//! ```

mod file;
mod open_options;
pub mod ops;

use std::{io, path::Path};

pub use file::File;
#[cfg(unix)]
pub use file::Metadata;
pub use open_options::OpenOptions;

const BUF_SIZE: usize = 64 * 1024;

/// Reads the entire contents of a file into a bytes vector.
///
/// # Examples
///
/// ```rust,no_run
/// use lio::fs;
///
/// async fn example() -> std::io::Result<()> {
///     let data = fs::read("/tmp/data.bin").await?;
///     println!("Read {} bytes", data.len());
///     Ok(())
/// }
/// ```
pub async fn read(path: impl AsRef<Path>) -> io::Result<Vec<u8>> {
  use crate::api;

  let file = File::open(path).await?;
  let mut result = Vec::new();

  loop {
    let buf = vec![0u8; BUF_SIZE];
    let (read_result, buf) = api::read(&file, buf).await;
    let n = read_result? as usize;

    if n == 0 {
      break;
    }

    result.extend_from_slice(&buf[..n]);
  }

  Ok(result)
}

/// Reads the entire contents of a file into a string.
///
/// # Errors
///
/// Returns an error if the file cannot be read or contains invalid UTF-8.
///
/// # Examples
///
/// ```rust,no_run
/// use lio::fs;
///
/// async fn example() -> std::io::Result<()> {
///     let contents = fs::read_to_string("/etc/hostname").await?;
///     println!("Hostname: {}", contents.trim());
///     Ok(())
/// }
/// ```
pub async fn read_to_string(path: impl AsRef<Path>) -> io::Result<String> {
  let bytes = read(path).await?;
  String::from_utf8(bytes).map_err(|e| {
    io::Error::new(
      io::ErrorKind::InvalidData,
      format!("file did not contain valid UTF-8: {}", e),
    )
  })
}

/// Writes data to a file, creating it if it doesn't exist.
///
/// This will overwrite the contents if the file exists.
///
/// # Examples
///
/// ```rust,no_run
/// use lio::fs;
///
/// async fn example() -> std::io::Result<()> {
///     fs::write("/tmp/output.txt", b"Hello, World!").await?;
///     Ok(())
/// }
/// ```
pub async fn write(
  path: impl AsRef<Path>,
  contents: impl AsRef<[u8]>,
) -> io::Result<()> {
  use crate::api;

  let file = File::create(path).await?;
  let buf = contents.as_ref().to_vec();
  let mut written = 0;
  let total = buf.len();

  while written < total {
    let to_write = buf[written..].to_vec();
    let (result, _) = api::write(&file, to_write).await;
    let n = result? as usize;

    if n == 0 {
      return Err(io::Error::new(
        io::ErrorKind::WriteZero,
        "failed to write whole buffer",
      ));
    }

    written += n;
  }

  Ok(())
}

/// Reads exactly `len` bytes from a file.
///
/// # Errors
///
/// Returns `UnexpectedEof` if EOF is reached before reading `len` bytes.
///
/// # Examples
///
/// ```rust,no_run
/// use lio::fs;
///
/// async fn example() -> std::io::Result<()> {
///     let header = fs::read_exact("/tmp/data.bin", 64).await?;
///     assert_eq!(header.len(), 64);
///     Ok(())
/// }
/// ```
pub async fn read_exact(
  path: impl AsRef<Path>,
  len: usize,
) -> io::Result<Vec<u8>> {
  use crate::api;

  let file = File::open(path).await?;
  let mut result = Vec::with_capacity(len);
  let mut remaining = len;

  while remaining > 0 {
    let buf = vec![0u8; remaining];
    let (read_result, buf) = api::read(&file, buf).await;
    let n = read_result? as usize;

    if n == 0 {
      return Err(io::Error::new(
        io::ErrorKind::UnexpectedEof,
        format!("expected {} bytes, got {}", len, result.len()),
      ));
    }

    result.extend_from_slice(&buf[..n]);
    remaining -= n;
  }

  Ok(result)
}
