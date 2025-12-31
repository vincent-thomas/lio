//! Resource management for file descriptors and handles.
//!
//! This module provides the [`Resource`] type, which is lio's wrapper around
//! platform-specific I/O resources (file descriptors on Unix, handles on Windows).

#[cfg(unix)]
use std::os::fd::{AsFd, BorrowedFd, FromRawFd, RawFd};
#[cfg(windows)]
use std::os::windows::io::RawHandle;
use std::sync::{
  Arc,
  atomic::{AtomicBool, Ordering},
};

#[cfg(unix)]
type Inner = RawFd;

#[cfg(windows)]
type Inner = RawHandle;

/// Internal owned resource with automatic cleanup support.
///
/// This type holds the actual platform resource (file descriptor or handle) and a flag
/// indicating whether it should be automatically closed when dropped.
struct Owned {
  /// The platform-specific resource (RawFd on Unix, RawHandle on Windows)
  inner: Inner,
  /// Whether to automatically close this resource on drop (not yet implemented)
  should_close: AtomicBool,
}

impl Owned {
  /// Creates a new owned resource.
  ///
  /// By default, `should_close` is false, meaning the resource will NOT be automatically
  /// closed on drop. This must be explicitly enabled via `Resource::close()`.
  fn new(inner: Inner) -> Self {
    Self { inner, should_close: AtomicBool::new(false) }
  }
}

impl Drop for Owned {
  /// Drops the owned resource, potentially closing it automatically.
  ///
  /// If `should_close` is true, this will close the underlying resource. Currently
  /// unimplemented and will panic if enabled.
  fn drop(&mut self) {
    if self.should_close.load(Ordering::Acquire) {
      todo!("run lio::close here");
    }
  }
}

/// A reference-counted OS-dependent I/O resource, whilst itself being
/// platform-independent.
///
/// # Resource Management
///
/// **NOTE**: `Resource` does **not** automatically
/// close the underlying file descriptor when dropped. You must explicitly call [`crate::close()`]
/// when you're done with the resource:
///
/// ```rust
/// use std::ffi::CString;
/// use lio::resource::Resource;
///
/// # lio::init();
/// async fn example() -> std::io::Result<()> {
///     let path = CString::new("/tmp/test").unwrap();
///     let fd: Resource = lio::openat(libc::AT_FDCWD, path, libc::O_RDONLY).await?;
///
///     // Use the resource...
///
///     // IMPORTANT: Close when done
///     fd.close();
///     Ok(())
/// }
/// # lio::exit();
/// ```
///
/// # Cloning and Sharing
///
/// `Resource` uses `Arc` internally, so cloning is cheap and creates a new reference to the
/// same underlying file descriptor:
///
/// ```rust
/// use lio::resource::Resource;
///
/// # lio::init();
/// async fn share_resource(fd: Resource) {
///     let fd_clone = fd.clone(); // Points to the same file descriptor
///
///     // Both fd and fd_clone refer to the same resource
///     // Only one close call is needed (the resource will be closed when all clones are dropped)
/// }
/// # lio::exit();
/// ```
///
/// # Creating Resources
///
/// Resources are typically obtained from lio I/O operations:
///
/// ```rust
/// use std::ffi::CString;
/// use lio::resource::Resource;
///
/// # lio::init();
/// async fn open_file() -> std::io::Result<Resource> {
///     let path = CString::new("/tmp/test").unwrap();
///     let resource: Resource = lio::openat(libc::AT_FDCWD, path, libc::O_RDONLY).await?;
///     Ok(resource)
/// }
///
/// async fn create_socket() -> std::io::Result<Resource> {
///     let sock: Resource = lio::socket(libc::AF_INET, libc::SOCK_STREAM, 0).await?;
///     Ok(sock)
/// }
/// # lio::exit();
/// ```
///
/// Or from raw file descriptors (Unix):
///
/// ```rust
/// use std::os::fd::{FromRawFd, RawFd};
/// use lio::resource::Resource;
///
/// # lio::init();
/// fn from_raw(raw_fd: RawFd) -> Resource {
///     // Safe if raw_fd is a valid, open file descriptor
///     unsafe { Resource::from_raw_fd(raw_fd) }
/// }
/// # lio::exit();
/// ```
#[derive(Clone)]
pub struct Resource(Arc<Owned>);

#[cfg(unix)]
impl FromRawFd for Resource {
  /// Creates a `Resource` from a raw file descriptor.
  ///
  /// # Safety
  ///
  /// The caller must ensure that:
  /// - `fd` is a valid, open file descriptor
  /// - No other code will close this file descriptor while the `Resource` exists
  /// - The file descriptor is not used in ways that conflict with lio's assumptions
  ///
  /// # Examples
  ///
  /// ```rust
  /// use std::os::fd::{FromRawFd, RawFd};
  /// use lio::resource::Resource;
  ///
  /// # lio::init();
  /// let fd: RawFd = 1; // stdout
  /// let resource = unsafe { Resource::from_raw_fd(fd) };
  /// # lio::exit();
  /// ```
  unsafe fn from_raw_fd(fd: RawFd) -> Self {
    Resource(Arc::new(Owned::new(fd)))
  }
}

#[cfg(unix)]
impl AsFd for Resource {
  fn as_fd(&self) -> BorrowedFd<'_> {
    unsafe { BorrowedFd::borrow_raw(self.0.inner) }
  }
}

// TODO: impl FromRawHandle + AsHandle

impl Resource {
  /// Marks this resource to be automatically closed when the last reference is dropped.
  ///
  /// After calling `close()`, when all clones of this `Resource` are dropped, the
  /// underlying file descriptor will be automatically closed via the `Drop` implementation.
  ///
  /// ```rust,ignore
  /// # use lio::resource::Resource;
  /// # lio::init();
  /// async fn future_example() -> std::io::Result<()> {
  ///     let fd = lio::openat(libc::AT_FDCWD, c"/tmp/test", libc::O_RDONLY).await?;
  ///     fd.close(); // Mark for auto-close
  ///     // fd is automatically closed when it goes out of scope
  ///     Ok(())
  /// }
  /// # lio::exit();
  /// ```
  fn close(&self) {
    self.0.should_close.store(true, Ordering::Release);
  }
}

impl std::fmt::Debug for Resource {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str("Resource")
  }
}
