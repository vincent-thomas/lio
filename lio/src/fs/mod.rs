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
//! use lio::fs::File;
//!
//! async fn read_example() -> std::io::Result<()> {
//!     let file = File::open("/tmp/example.txt").await?;
//!     let buffer = vec![0u8; 1024];
//!     let (result, buffer) = file.read(buffer).await;
//!     let bytes_read = result? as usize;
//!     println!("Read {} bytes: {:?}", bytes_read, &buffer[..bytes_read]);
//!     Ok(())
//! }
//! ```
//!
//! ## Writing a file
//!
//! ```rust,no_run
//! use lio::fs::File;
//!
//! async fn write_example() -> std::io::Result<()> {
//!     let file = File::create("/tmp/output.txt").await?;
//!     let data = b"Hello, World!".to_vec();
//!     let (result, _) = file.write(data).await;
//!     result?;
//!     file.sync_all().await?;
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

pub use file::File;
#[cfg(unix)]
pub use file::Metadata;
pub use open_options::OpenOptions;
