//! Resource management for file descriptors and handles.
//!
//! This module provides the [`Resource`] type, which is lio's wrapper around
//! platform-specific I/O resources (file descriptors on Unix, handles on Windows).
//!
//! ```rust
//! use std::ffi::CString;
//! use lio::resource::Resource;
//!
//! # lio::init();
//! async fn example() -> std::io::Result<()> {
//!     let path = CString::new("/tmp/test").unwrap();
//!     let fd: Resource = lio::openat(libc::AT_FDCWD, path, libc::O_RDONLY).await?;
//!
//!     // Use the resource...
//!
//!     // IMPORTANT: Close when done
//!     fd.close();
//!     Ok(())
//! }
//! # lio::exit();
//! ```
//!
//! # Cloning and Sharing
//!
//! Cloning [`Resource`]'s is very cheap and all clones point to the same
//! underlying file descriptor/handle:
//!
//! ```rust
//! use lio::resource::Resource;
//!
//! # lio::init();
//! async fn share_resource(fd: Resource) {
//!     let fd_clone = fd.clone(); // Points to the same file descriptor
//!
//!     // Both fd and fd_clone refer to the same resource
//!     // Only one close call is needed (the resource will be closed when all clones are dropped)
//! }
//! # lio::exit();
//! ```
//!
//! # Creating Resources
//!
//! Resources are typically obtained from lio I/O operations:
//!
//! ```rust
//! use std::ffi::CString;
//! use lio::resource::Resource;
//!
//! # lio::init();
//! async fn open_file() -> std::io::Result<Resource> {
//!     let path = CString::new("/tmp/test").unwrap();
//!     let resource: Resource = lio::openat(libc::AT_FDCWD, path, libc::O_RDONLY).await?;
//!     Ok(resource)
//! }
//!
//! async fn create_socket() -> std::io::Result<Resource> {
//!     let sock: Resource = lio::socket(libc::AF_INET, libc::SOCK_STREAM, 0).await?;
//!     Ok(sock)
//! }
//! # lio::exit();
//! ```
//!
//! Or from raw file descriptors (Unix):
//!
//! ```rust
//! use std::os::fd::{FromRawFd, RawFd};
//! use lio::resource::Resource;
//!
//! # lio::init();
//! fn from_raw(raw_fd: RawFd) -> Resource {
//!     // Safe if raw_fd is a valid, open file descriptor
//!     unsafe { Resource::from_raw_fd(raw_fd) }
//! }
//! # lio::exit();
//! ```
//!
//! # Unique Resources
//!
//! When you need exclusive ownership of a resource without the reference counting overhead,
//! use [`UniqueResource`]. This type guarantees that only one owner exists at a time,
//! making it suitable for scenarios where sharing is not required.
//!
//! You can convert between [`Resource`] and [`UniqueResource`]:
//!
//! ```rust
//! use std::ffi::CString;
//! use lio::resource::Resource;
//!
//! # lio::init();
//! async fn example() -> std::io::Result<()> {
//!     let path = CString::new("/tmp/test").unwrap();
//!     let fd: Resource = lio::openat(libc::AT_FDCWD, path, libc::O_RDONLY).await?;
//!
//!     // Convert to UniqueResource if you have the only reference
//!     match fd.into_unique() {
//!         Ok(unique_fd) => {
//!             // Now you have exclusive ownership
//!             // unique_fd can be converted back to Resource if needed
//!             let fd: Resource = unique_fd.into();
//!             fd.close();
//!         }
//!         Err(shared_fd) => {
//!             // Conversion failed because other references exist
//!             shared_fd.close();
//!         }
//!     }
//!     Ok(())
//! }
//! # lio::exit();
//! ```

use std::sync::{
  Arc,
  atomic::{AtomicBool, Ordering},
};

macro_rules! impl_native_convervions {
  ($nice:ident) => {
    #[cfg(unix)]
    impl std::os::fd::AsFd for $nice {
      fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        unsafe { std::os::fd::BorrowedFd::borrow_raw(self.0.inner) }
      }
    }

    #[cfg(unix)]
    impl std::os::fd::AsRawFd for $nice {
      fn as_raw_fd(&self) -> std::os::fd::RawFd {
        self.0.inner
      }
    }

    #[cfg(windows)]
    impl std::os::windows::io::AsHandle for $nice {
      fn as_handle(&self) -> std::os::windows::io::BorrowedHandle<'_> {
        unsafe {
          std::os::windows::io::BorrowedHandle::borrow_raw(self.0.inner)
        }
      }
    }

    #[cfg(windows)]
    impl std::os::windows::io::AsRawHandle for $nice {
      fn as_raw_handle(&self) -> RawHandle {
        self.0.inner
      }
    }
  };
}

#[cfg(unix)]
type Inner = std::os::fd::RawFd;

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
    Self { inner, should_close: AtomicBool::new(true) }
  }
}

impl Drop for Owned {
  /// Drops the owned resource, potentially closing it automatically.
  ///
  /// If `should_close` is true, this will close the underlying resource. Currently
  /// unimplemented and will panic if enabled.
  fn drop(&mut self) {
    if self.should_close.load(Ordering::Acquire) {
      let op = crate::close(UniqueResource(Owned {
        inner: self.inner,
        should_close: AtomicBool::new(true),
      }));

      // NOTE: Is this really neccessary?
      let _ = op.blocking();
    }
  }
}

/// A reference-counted, platform-independent wrapper around OS I/O resources.
///
/// See the [module documentation](self) for usage examples and details.
#[derive(Clone)]
pub struct Resource(Arc<Owned>);

impl From<&Resource> for Resource {
  fn from(value: &Resource) -> Resource {
    value.clone()
  }
}

impl From<UniqueResource> for Resource {
  fn from(value: UniqueResource) -> Self {
    Resource(Arc::new(value.0))
  }
}

impl_native_convervions!(Resource);

#[cfg(unix)]
impl std::os::fd::FromRawFd for Resource {
  unsafe fn from_raw_fd(fd: std::os::fd::RawFd) -> Self {
    Resource(Arc::new(Owned::new(fd)))
  }
}

#[cfg(windows)]
impl std::os::windows::io::FromRawHandle for Resource {
  unsafe fn from_raw_handle(handle: RawHandle) -> Self {
    Resource(Arc::new(Owned::new(handle)))
  }
}

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
  ///     fd.mark_close(); // Mark for auto-close
  ///     // fd is automatically closed when it goes out of scope
  ///     Ok(())
  /// }
  /// # lio::exit();
  /// ```
  pub fn dont_close(&self) {
    self.0.should_close.store(false, Ordering::Release);
  }

  pub fn count(&self) -> usize {
    Arc::strong_count(&self.0)
  }

  pub fn into_unique(self) -> Result<UniqueResource, Resource> {
    match Arc::try_unwrap(self.0) {
      Ok(owner) => Ok(UniqueResource(owner)),
      Err(e) => Err(Resource(e)),
    }
  }
}

impl std::fmt::Debug for Resource {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str("Resource")
  }
}

/// A uniquely-owned, platform-independent wrapper around OS I/O resources.
///
/// See the [module-level documentation on unique resources](self#unique-resources) for examples.
pub struct UniqueResource(Owned);

impl_native_convervions!(UniqueResource);

#[cfg(unix)]
impl std::os::fd::FromRawFd for UniqueResource {
  unsafe fn from_raw_fd(fd: std::os::fd::RawFd) -> Self {
    UniqueResource(Owned::new(fd))
  }
}

#[cfg(windows)]
impl std::os::windows::io::FromRawHandle for UniqueResource {
  unsafe fn from_raw_handle(handle: RawHandle) -> Self {
    UniqueResource(Owned::new(handle))
  }
}
