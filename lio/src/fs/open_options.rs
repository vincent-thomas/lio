//! Options and flags for configuring file opening.

use std::{io, path::Path};

use crate::{api::io::Io, fs::ops::OpenAtFile};

/// Options and flags which can be used to configure how a file is opened.
///
/// This builder exposes the ability to configure how a [`File`](super::File) is opened
/// and what operations are permitted on the open file.
///
/// # Examples
///
/// ## Opening a file for reading
///
/// ```rust,no_run
/// use lio::fs::OpenOptions;
///
/// async fn example() -> std::io::Result<()> {
///     let file = OpenOptions::new()
///         .read(true)
///         .open("/etc/hosts")
///         .await?;
///     Ok(())
/// }
/// ```
///
/// ## Creating a file for writing
///
/// ```rust,no_run
/// use lio::fs::OpenOptions;
///
/// async fn example() -> std::io::Result<()> {
///     let file = OpenOptions::new()
///         .write(true)
///         .create(true)
///         .truncate(true)
///         .open("/tmp/output.txt")
///         .await?;
///     Ok(())
/// }
/// ```
///
/// ## Opening for append
///
/// ```rust,no_run
/// use lio::fs::OpenOptions;
///
/// async fn example() -> std::io::Result<()> {
///     let file = OpenOptions::new()
///         .append(true)
///         .open("/var/log/app.log")
///         .await?;
///     Ok(())
/// }
/// ```
///
/// ## Opening with custom mode
///
/// ```rust,no_run
/// use lio::fs::OpenOptions;
///
/// async fn example() -> std::io::Result<()> {
///     let file = OpenOptions::new()
///         .write(true)
///         .create(true)
///         .mode(0o600)  // Owner read/write only
///         .open("/tmp/secret.txt")
///         .await?;
///     Ok(())
/// }
/// ```
#[derive(Debug, Clone)]
pub struct OpenOptions {
  read: bool,
  write: bool,
  append: bool,
  truncate: bool,
  create: bool,
  create_new: bool,
  #[cfg(unix)]
  mode: u32,
}

impl OpenOptions {
  /// Creates a blank new set of options ready for configuration.
  ///
  /// All options are initially set to `false`.
  #[must_use]
  pub const fn new() -> Self {
    Self {
      read: false,
      write: false,
      append: false,
      truncate: false,
      create: false,
      create_new: false,
      #[cfg(unix)]
      mode: 0o666,
    }
  }

  /// Sets the option for read access.
  ///
  /// This option, when true, will indicate that the file should be
  /// readable if opened.
  #[must_use]
  pub const fn read(mut self, read: bool) -> Self {
    self.read = read;
    self
  }

  /// Sets the option for write access.
  ///
  /// This option, when true, will indicate that the file should be
  /// writable if opened.
  #[must_use]
  pub const fn write(mut self, write: bool) -> Self {
    self.write = write;
    self
  }

  /// Sets the option for append mode.
  ///
  /// This option, when true, means that writes will append to a file instead
  /// of overwriting previous contents. Note that setting `.write(true).append(true)`
  /// has the same effect as setting only `.append(true)`.
  #[must_use]
  pub const fn append(mut self, append: bool) -> Self {
    self.append = append;
    self
  }

  /// Sets the option for truncating a previous file.
  ///
  /// If a file is successfully opened with this option set it will truncate
  /// the file to 0 length if it already exists.
  #[must_use]
  pub const fn truncate(mut self, truncate: bool) -> Self {
    self.truncate = truncate;
    self
  }

  /// Sets the option to create a new file, or open it if it already exists.
  ///
  /// In order for the file to be created, `.write(true)` or `.append(true)`
  /// must also be used.
  #[must_use]
  pub const fn create(mut self, create: bool) -> Self {
    self.create = create;
    self
  }

  /// Sets the option to create a new file, failing if it already exists.
  ///
  /// No file is allowed to exist at the target location, also no (dangling) symlink.
  /// In this way, if the call succeeds, the file returned is guaranteed to be new.
  ///
  /// This option is useful because it is atomic. Otherwise between checking whether
  /// a file exists and creating a new one, the file may have been created by another
  /// process (a TOCTOU race condition / attack).
  #[must_use]
  pub const fn create_new(mut self, create_new: bool) -> Self {
    self.create_new = create_new;
    self
  }

  /// Sets the mode bits that a new file will be created with.
  ///
  /// If a new file is created as part of an `open` call, it will be created with
  /// the permissions specified by `mode` (modified by the process umask).
  /// The default mode is `0o666`.
  ///
  /// # Platform-specific behavior
  ///
  /// This option is only available on Unix platforms. The mode is subject to
  /// the process umask.
  #[cfg(unix)]
  #[must_use]
  pub const fn mode(mut self, mode: u32) -> Self {
    self.mode = mode;
    self
  }

  /// Opens a file at `path` with the options specified by `self`.
  ///
  /// # Errors
  ///
  /// This function will return an error if:
  /// - No access mode is specified (at least one of `read`, `write`, or `append` must be set)
  /// - The file does not exist and `create` is not set
  /// - The user lacks permission to access the file
  /// - `create_new` is set and the file already exists
  ///
  /// # Examples
  ///
  /// ```rust,no_run
  /// use lio::fs::OpenOptions;
  ///
  /// async fn example() -> std::io::Result<()> {
  ///     let file = OpenOptions::new()
  ///         .read(true)
  ///         .write(true)
  ///         .create(true)
  ///         .open("/tmp/example.txt")
  ///         .await?;
  ///     Ok(())
  /// }
  /// ```
  pub fn open<P: AsRef<Path>>(&self, path: P) -> Io<OpenAtFile> {
    OpenAtFile::open(self, path.as_ref())
  }

  /// Converts the options into Unix open flags.
  #[cfg(unix)]
  pub(crate) fn to_flags(&self) -> io::Result<(libc::c_int, libc::mode_t)> {
    let mut flags: libc::c_int = 0;

    // Determine access mode
    match (self.read, self.write, self.append) {
      (true, false, false) => flags |= libc::O_RDONLY,
      (false, true, false) => flags |= libc::O_WRONLY,
      (true, true, false) => flags |= libc::O_RDWR,
      (false, false, true) => flags |= libc::O_WRONLY | libc::O_APPEND,
      (true, false, true) => flags |= libc::O_RDWR | libc::O_APPEND,
      (false, true, true) => flags |= libc::O_WRONLY | libc::O_APPEND,
      (true, true, true) => flags |= libc::O_RDWR | libc::O_APPEND,
      (false, false, false) => {
        return Err(io::Error::new(
          io::ErrorKind::InvalidInput,
          "at least one of read, write, or append must be set",
        ));
      }
    }

    // Creation flags
    if self.create_new {
      flags |= libc::O_CREAT | libc::O_EXCL;
    } else if self.create {
      flags |= libc::O_CREAT;
    }

    // Truncate
    if self.truncate {
      flags |= libc::O_TRUNC;
    }

    Ok((flags, self.mode as libc::mode_t))
  }
}

impl Default for OpenOptions {
  fn default() -> Self {
    Self::new()
  }
}
