//! The [`File`] type for filesystem I/O.

use std::{io, path::Path};

use crate::{
  api::{
    self,
    io::Io,
    ops::{Fsync, ReadV, ReadVAt, Truncate, WriteV, WriteVAt},
    resource::{AsResource, FromResource, IntoResource, Resource},
  },
  buf::{IoBuf, IoBufMut},
  fs::{OpenOptions, ops::OpenAtFile},
};

/// A reference to an open file on the filesystem.
///
/// `File` provides async I/O operations on an open file descriptor. Files are
/// opened using the [`open`](File::open), [`create`](File::create), or
/// [`create_new`](File::create_new) methods, or via [`OpenOptions`] for more
/// control.
///
/// # Examples
///
/// ## Reading a file
///
/// ```rust,no_run
/// use lio::fs::File;
///
/// async fn read_file() -> std::io::Result<()> {
///     let file = File::open("/etc/passwd").await?;
///     let buffer = vec![0u8; 4096];
///     let (result, buffer) = file.read(buffer).await;
///     let bytes_read = result? as usize;
///     println!("Read {} bytes", bytes_read);
///     Ok(())
/// }
/// ```
///
/// ## Writing a file
///
/// ```rust,no_run
/// use lio::fs::File;
///
/// async fn write_file() -> std::io::Result<()> {
///     let file = File::create("/tmp/output.txt").await?;
///     let data = b"Hello, world!".to_vec();
///     let (result, _data) = file.write(data).await;
///     let bytes_written = result?;
///     println!("Wrote {} bytes", bytes_written);
///     Ok(())
/// }
/// ```
///
/// ## Positional I/O
///
/// ```rust,no_run
/// use lio::fs::File;
///
/// async fn positional_io() -> std::io::Result<()> {
///     let file = File::open("/tmp/data.bin").await?;
///
///     // Read 100 bytes starting at offset 1024
///     let buffer = vec![0u8; 100];
///     let (result, buffer) = file.read_at(buffer, 1024).await;
///     let bytes_read = result?;
///
///     Ok(())
/// }
/// ```
#[derive(Debug)]
pub struct File(Resource);

impl IntoResource for File {
  fn into_resource(self) -> Resource {
    self.0
  }
}

impl AsResource for File {
  fn as_resource(&self) -> &Resource {
    &self.0
  }
}

impl FromResource for File {
  fn from_resource(resource: Resource) -> Self {
    Self(resource)
  }
}

impl File {
  /// Opens a file in read-only mode.
  ///
  /// This is equivalent to `OpenOptions::new().read(true).open(path)`.
  ///
  /// # Errors
  ///
  /// Returns an error if the file does not exist or the user lacks permission
  /// to read it.
  ///
  /// # Examples
  ///
  /// ```rust,no_run
  /// use lio::fs::File;
  ///
  /// async fn example() -> std::io::Result<()> {
  ///     let file = File::open("/etc/hosts").await?;
  ///     Ok(())
  /// }
  /// ```
  pub fn open(path: impl AsRef<Path>) -> Io<OpenAtFile> {
    OpenOptions::new().read(true).open(path)
  }

  /// Opens a file in write-only mode, creating it if it doesn't exist,
  /// and truncating it if it does.
  ///
  /// This is equivalent to `OpenOptions::new().write(true).create(true).truncate(true).open(path)`.
  ///
  /// # Examples
  ///
  /// ```rust,no_run
  /// use lio::fs::File;
  ///
  /// async fn example() -> std::io::Result<()> {
  ///     let file = File::create("/tmp/output.txt").await?;
  ///     Ok(())
  /// }
  /// ```
  pub fn create(path: impl AsRef<Path>) -> Io<OpenAtFile> {
    OpenOptions::new().write(true).create(true).truncate(true).open(path)
  }

  /// Creates a new file in read-write mode; error if the file exists.
  ///
  /// This is equivalent to `OpenOptions::new().read(true).write(true).create_new(true).open(path)`.
  ///
  /// This option is useful for ensuring that the file is new. If the file
  /// already exists, an error is returned.
  ///
  /// # Examples
  ///
  /// ```rust,no_run
  /// use lio::fs::File;
  ///
  /// async fn example() -> std::io::Result<()> {
  ///     let file = File::create_new("/tmp/new_file.txt").await?;
  ///     Ok(())
  /// }
  /// ```
  pub fn create_new(path: impl AsRef<Path>) -> Io<OpenAtFile> {
    OpenOptions::new().read(true).write(true).create_new(true).open(path)
  }

  /// Reads data from the file into the provided buffer.
  ///
  /// The buffer is returned along with the number of bytes read. The file's
  /// internal cursor position is advanced by the number of bytes read.
  ///
  /// # Returns
  ///
  /// A tuple containing:
  /// - `io::Result<i32>`: The number of bytes read, or an error
  /// - The buffer (returned for reuse)
  ///
  /// # Examples
  ///
  /// ```rust,no_run
  /// use lio::fs::File;
  ///
  /// async fn example() -> std::io::Result<()> {
  ///     let file = File::open("/tmp/data.txt").await?;
  ///     let buffer = vec![0u8; 1024];
  ///     let (result, buffer) = file.read(buffer).await;
  ///     let bytes_read = result? as usize;
  ///     println!("Read: {:?}", &buffer[..bytes_read]);
  ///     Ok(())
  /// }
  /// ```
  pub fn read<B: IoBufMut>(&self, buf: B) -> Io<ReadV<B>> {
    api::read(&self.0, buf)
  }

  /// Reads data from the file at a specific offset without changing the
  /// file's internal cursor position.
  ///
  /// This is useful for random access patterns where you need to read from
  /// multiple positions without seeking.
  ///
  /// # Parameters
  ///
  /// - `buf`: The buffer to read into
  /// - `offset`: The byte offset from the start of the file
  ///
  /// # Returns
  ///
  /// A tuple containing:
  /// - `io::Result<i32>`: The number of bytes read, or an error
  /// - The buffer (returned for reuse)
  ///
  /// # Examples
  ///
  /// ```rust,no_run
  /// use lio::fs::File;
  ///
  /// async fn example() -> std::io::Result<()> {
  ///     let file = File::open("/tmp/data.bin").await?;
  ///     // Read 100 bytes starting at offset 1024
  ///     let buffer = vec![0u8; 100];
  ///     let (result, buffer) = file.read_at(buffer, 1024).await;
  ///     Ok(())
  /// }
  /// ```
  pub fn read_at<B: IoBufMut>(&self, buf: B, offset: i64) -> Io<ReadVAt<B>> {
    api::read_at(&self.0, buf, offset)
  }

  /// Writes data from the buffer to the file.
  ///
  /// The buffer is returned along with the number of bytes written. The file's
  /// internal cursor position is advanced by the number of bytes written.
  ///
  /// # Returns
  ///
  /// A tuple containing:
  /// - `io::Result<i32>`: The number of bytes written, or an error
  /// - The buffer (returned for reuse)
  ///
  /// # Examples
  ///
  /// ```rust,no_run
  /// use lio::fs::File;
  ///
  /// async fn example() -> std::io::Result<()> {
  ///     let file = File::create("/tmp/output.txt").await?;
  ///     let data = b"Hello, World!".to_vec();
  ///     let (result, _data) = file.write(data).await;
  ///     let bytes_written = result?;
  ///     println!("Wrote {} bytes", bytes_written);
  ///     Ok(())
  /// }
  /// ```
  pub fn write<B: IoBuf>(&self, buf: B) -> Io<WriteV<B>> {
    api::write(&self.0, buf)
  }

  /// Writes data to the file at a specific offset without changing the
  /// file's internal cursor position.
  ///
  /// This is useful for random access patterns where you need to write to
  /// multiple positions without seeking.
  ///
  /// # Parameters
  ///
  /// - `buf`: The buffer containing data to write
  /// - `offset`: The byte offset from the start of the file
  ///
  /// # Returns
  ///
  /// A tuple containing:
  /// - `io::Result<i32>`: The number of bytes written, or an error
  /// - The buffer (returned for reuse)
  ///
  /// # Examples
  ///
  /// ```rust,no_run
  /// use lio::fs::File;
  ///
  /// async fn example() -> std::io::Result<()> {
  ///     let file = File::create("/tmp/data.bin").await?;
  ///     let data = b"random access write".to_vec();
  ///     // Write at offset 1024
  ///     let (result, _data) = file.write_at(data, 1024).await;
  ///     Ok(())
  /// }
  /// ```
  pub fn write_at<B: IoBuf>(&self, buf: B, offset: i64) -> Io<WriteVAt<B>> {
    api::write_at(&self.0, buf, offset)
  }

  /// Synchronizes all file data and metadata to disk.
  ///
  /// This ensures that all data and metadata (like timestamps and permissions)
  /// are written to the underlying storage device.
  ///
  /// # Examples
  ///
  /// ```rust,no_run
  /// use lio::fs::File;
  ///
  /// async fn example() -> std::io::Result<()> {
  ///     let file = File::create("/tmp/important.txt").await?;
  ///     let data = b"critical data".to_vec();
  ///     let (result, _) = file.write(data).await;
  ///     result?;
  ///
  ///     // Ensure data is persisted
  ///     file.sync_all().await?;
  ///     Ok(())
  /// }
  /// ```
  pub fn sync_all(&self) -> Io<Fsync> {
    api::fsync(&self.0)
  }

  /// Truncates or extends the file to the specified length.
  ///
  /// If the file is larger than `size`, the extra data is lost. If the file
  /// is smaller than `size`, it is extended with zeros.
  ///
  /// # Parameters
  ///
  /// - `size`: The new size of the file in bytes
  ///
  /// # Examples
  ///
  /// ```rust,no_run
  /// use lio::fs::File;
  ///
  /// async fn example() -> std::io::Result<()> {
  ///     let file = File::create("/tmp/fixed_size.bin").await?;
  ///     // Allocate exactly 1MB
  ///     file.set_len(1024 * 1024).await?;
  ///     Ok(())
  /// }
  /// ```
  pub fn set_len(&self, size: u64) -> Io<Truncate> {
    api::truncate(&self.0, size)
  }

  /// Returns metadata about the file.
  ///
  /// This performs an `fstat` syscall to get file metadata synchronously.
  ///
  /// # Examples
  ///
  /// ```rust,no_run
  /// use lio::fs::File;
  ///
  /// async fn example() -> std::io::Result<()> {
  ///     let file = File::open("/tmp/example.txt").await?;
  ///     let metadata = file.metadata()?;
  ///     println!("File size: {} bytes", metadata.len());
  ///     Ok(())
  /// }
  /// ```
  #[cfg(unix)]
  pub fn metadata(&self) -> io::Result<Metadata> {
    use std::os::fd::AsRawFd;
    let fd = self.0.as_raw_fd();

    // SAFETY: stat struct can be safely zero-initialized
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };

    // SAFETY: fd is valid, stat is a valid mutable pointer
    let ret = unsafe { libc::fstat(fd, &mut stat) };

    if ret < 0 {
      return Err(io::Error::last_os_error());
    }

    Ok(Metadata { stat })
  }
}

/// Metadata information about a file.
///
/// This structure is returned from [`File::metadata`] and provides access
/// to file attributes like size, timestamps, and permissions.
#[cfg(unix)]
pub struct Metadata {
  stat: libc::stat,
}

#[cfg(unix)]
impl Metadata {
  /// Returns the size of the file in bytes.
  pub fn len(&self) -> u64 {
    self.stat.st_size as u64
  }

  /// Returns true if the file has zero size.
  pub fn is_empty(&self) -> bool {
    self.len() == 0
  }

  /// Returns true if this metadata is for a regular file.
  pub fn is_file(&self) -> bool {
    (self.stat.st_mode & libc::S_IFMT) == libc::S_IFREG
  }

  /// Returns true if this metadata is for a directory.
  pub fn is_dir(&self) -> bool {
    (self.stat.st_mode & libc::S_IFMT) == libc::S_IFDIR
  }

  /// Returns true if this metadata is for a symbolic link.
  pub fn is_symlink(&self) -> bool {
    (self.stat.st_mode & libc::S_IFMT) == libc::S_IFLNK
  }

  /// Returns the permissions of the file (Unix mode bits).
  pub fn permissions(&self) -> u32 {
    (self.stat.st_mode & 0o7777) as u32
  }

  /// Returns the raw stat structure for advanced usage.
  pub fn stat(&self) -> &libc::stat {
    &self.stat
  }
}
