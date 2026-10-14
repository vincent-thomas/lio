//! Resource management for file descriptors and handles.
//!
//! This module provides the [`Resource`] type, which is lio's abstraction around
//! platform-specific I/O resources (file descriptors on Unix and some other platforms, handles on Windows).
//!
//! ```rust,no_run
//! use std::ffi::CString;
//! use lio::api::resource::Resource;
//!
//! async fn example() -> std::io::Result<()> {
//!     # let dir = Resource::cwd();
//!     let path = CString::new("/tmp/test").unwrap();
//!     let fd: Resource =
//!         lio::api::openat(&dir, path, lio::api::OpenFlags::READ, 0).await?;
//!
//!     // Use the resource...
//!     // Resource is automatically closed when dropped.
//!     Ok(())
//! }
//! ```
//!
//! # Cloning and Sharing
//!
//! Cloning [`Resource`]'s is very cheap, all clones point to the same
//! underlying file descriptor/handle:
//!
//! ```rust
//! use lio::api::resource::Resource;
//!
//! async fn share_resource(fd: Resource) {
//!     let fd_clone = fd.clone(); // Points to the same file descriptor
//!
//!     // Both fd and fd_clone refer to the same resource.
//!     // The resource is closed when all clones are dropped.
//! }
//! ```
//!
//! # Creating Resources
//!
//! Resources are typically obtained from lio I/O operations:
//!
//! ```rust,no_run
//! use std::ffi::CString;
//! use lio::api::resource::Resource;
//!
//! async fn open_file() -> std::io::Result<Resource> {
//!     # let dir = Resource::cwd();
//!     let path = CString::new("/tmp/test").unwrap();
//!     let resource: Resource =
//!         lio::api::openat(&dir, path, lio::api::OpenFlags::READ, 0).await?;
//!     Ok(resource)
//! }
//!
//! async fn create_socket() -> std::io::Result<Resource> {
//!     let sock: Resource = lio::api::socket(
//!         lio::api::SockDomain::IPV4,
//!         lio::api::SockType::STREAM,
//!         lio::api::SockProto::DEFAULT,
//!     ).await?;
//!     Ok(sock)
//! }
//! ```
//!
//! Or from raw file descriptors (Unix):
//!
//! ```rust
//! # #[cfg(unix)]
//! use std::os::fd::{FromRawFd, RawFd};
//! use lio::api::resource::Resource;
//!
//! # #[cfg(unix)]
//! fn from_raw(raw_fd: RawFd) -> Resource {
//!     // Safe if raw_fd is a valid, open file descriptor
//!     unsafe { Resource::from_raw_fd(raw_fd) }
//! }
//! ```

use std::io;
use std::sync::Arc;

macro_rules! impl_native_convervions {
  ($nice:ident) => {
    #[cfg(unix)]
    impl std::os::fd::AsFd for $nice {
      fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        // SAFETY: Owned guarrantees it's valid.
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
        // SAFETY: the handle stays open for as long as `self` is borrowed.
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

/// The native value behind a [`Resource`]: a file descriptor on Unix, a
/// handle (or socket) on Windows.
#[cfg(unix)]
pub type RawResource = std::os::fd::RawFd;

/// The native value behind a [`Resource`]: a file descriptor on Unix, a
/// handle (or socket) on Windows.
#[cfg(windows)]
pub type RawResource = std::os::windows::io::RawHandle;

#[cfg(windows)]
use std::os::windows::io::RawHandle;

type Inner = RawResource;

/// Converts a successful completion result into the descriptor it carries,
/// rejecting values that are negative or out of range.
#[cfg(unix)]
fn raw_from_result(result: isize) -> Option<RawResource> {
  RawResource::try_from(result).ok().filter(|fd| *fd >= 0)
}

/// Converts a successful completion result into the handle it carries,
/// rejecting the null and pseudo-handle range.
#[cfg(windows)]
fn raw_from_result(result: isize) -> Option<RawResource> {
  (result > 0).then_some(result as RawResource)
}

/// Closes a [`RawResource`] its caller owns.
///
/// Every owned [`Resource`] carries the close function of whoever created it:
/// the backend for resources produced by operations (see
/// [`IoBackend::resource_close`](crate::backend::IoBackend::resource_close)),
/// or the host OS's native close for resources adopted from raw
/// descriptors/handles.
pub type CloseFn = unsafe fn(RawResource);

// SAFETY: a raw handle is an opaque identifier; like `OwnedHandle`, owning
// or sharing it across threads is sound.
#[cfg(windows)]
unsafe impl Send for InnerResource {}
// SAFETY: see `Send` above; `InnerResource` has no interior mutability.
#[cfg(windows)]
unsafe impl Sync for InnerResource {}

/// Internal resource with automatic cleanup support.
struct InnerResource {
  /// The platform-specific resource (RawFd on Unix, RawHandle on Windows)
  inner: Inner,
  /// How to close `inner` on drop; `None` for borrowed resources.
  close: Option<CloseFn>,
}

impl InnerResource {
  /// Creates a new owned resource that `close` releases on drop.
  fn new(inner: Inner, close: CloseFn) -> Self {
    Self { inner, close: Some(close) }
  }

  /// Creates a new borrowed resource that will NOT be closed on drop.
  fn new_borrowed(inner: Inner) -> Self {
    Self { inner, close: None }
  }
}

impl Drop for InnerResource {
  /// Closes the resource with its owner's close function, if it is owned.
  fn drop(&mut self) {
    if let Some(close) = self.close {
      // SAFETY: this resource owns `inner` and is dropped exactly once.
      unsafe { close(self.inner) };
    }
  }
}

/// A reference-counted, platform-independent wrapper around OS I/O resources.
///
/// See the [module documentation](self) for usage examples and details.
#[derive(Clone)]
pub struct Resource(Arc<InnerResource>);

/// Trait for types that can be converted into a [`Resource`].
///
/// This trait is implemented by high-level types like `Socket`, `TcpSocket`, and
/// `TcpListener` (in the `net` module, requires `high` feature) to allow conversion
/// to the underlying resource.
///
/// # Examples
///
/// ```rust,ignore
/// use lio::api::resource::{Resource, IntoResource};
/// use lio::net::Socket;
///
/// fn take_resource(res: impl IntoResource) {
///     let resource: Resource = res.into_resource();
///     // Use the resource...
/// }
///
/// let socket = Socket::new(lio::api::SockDomain::IPV4, lio::api::SockType::STREAM, lio::api::SockProto::DEFAULT).await?;
/// take_resource(socket);
/// ```
pub trait IntoResource {
  /// Consumes `self` and returns the underlying [`Resource`].
  ///
  /// This transfers ownership of the resource to the caller.
  fn into_resource(self) -> Resource;
}

/// Trait for types that can provide a reference to their underlying [`Resource`].
///
/// This trait is implemented by high-level types that wrap a [`Resource`] and need
/// to provide temporary access to it without transferring ownership.
///
/// # Examples
///
/// ```rust,ignore
/// use lio::api::resource::{Resource, AsResource};
/// use lio::net::Socket;
///
/// fn inspect_resource(obj: &impl AsResource) {
///     let resource: &Resource = obj.as_resource();
///     println!("Resource count: {}", resource.count());
/// }
///
/// let socket = Socket::new(lio::api::SockDomain::IPV4, lio::api::SockType::STREAM, lio::api::SockProto::DEFAULT).await?;
/// inspect_resource(&socket);
/// // socket is still usable here
/// ```
pub trait AsResource {
  /// Returns a reference to the underlying [`Resource`].
  ///
  /// This borrows the resource without transferring ownership.
  fn as_resource(&self) -> &Resource;
}

/// Direct implementation for [`Resource`] itself.
///
/// This allows using a `Resource` directly where `AsResource` is expected.
impl AsResource for &Resource {
  fn as_resource(&self) -> &Resource {
    self
  }
}

impl AsResource for Resource {
  fn as_resource(&self) -> &Resource {
    self
  }
}

/// Trait for types that can be constructed from a [`Resource`].
///
/// This trait is implemented by high-level types like `Socket`, `TcpSocket`, and
/// `TcpListener` (in the `net` module, requires `high` feature) to allow wrapping
/// a raw [`Resource`] in a typed interface.
///
/// # Safety Contract
///
/// Implementors guarantee that the [`Resource`] is compatible with the type being
/// constructed. For example, constructing a `TcpSocket` from a resource requires
/// that the resource is actually a TCP socket file descriptor.
pub trait FromResource {
  /// Creates `Self` from a [`Resource`].
  ///
  /// # Safety Contract
  ///
  /// The caller must ensure that the `resource` is compatible with the type
  /// being created. For example, creating a TCP socket wrapper requires that
  /// the resource is actually a TCP socket.
  ///
  /// Violating this contract may lead to incorrect behavior when operations
  /// are performed on the wrapped resource.
  fn from_resource(resource: Resource) -> Self;
}

impl_native_convervions!(Resource);

#[cfg(unix)]
impl std::os::fd::FromRawFd for Resource {
  unsafe fn from_raw_fd(fd: std::os::fd::RawFd) -> Self {
    Resource(Arc::new(InnerResource::new(fd, crate::backend::native::close)))
  }
}

impl Resource {
  /// Takes ownership of the descriptor/handle a resource-creating
  /// operation completed with.
  ///
  /// The resource is closed with the close function of the backend that
  /// produced the completion.
  ///
  /// # Safety
  ///
  /// A non-negative `completion.result` must carry a newly created
  /// descriptor/handle owned by the caller.
  ///
  /// # Errors
  ///
  /// A negative result is the operation's error code. A non-negative result
  /// that does not fit a [`RawResource`] is reported as
  /// [`io::ErrorKind::InvalidData`] instead of being truncated.
  pub(crate) unsafe fn from_completion(
    completion: &crate::api::op::Completion,
  ) -> io::Result<Self> {
    if completion.result < 0 {
      return Err(io::Error::from_raw_os_error((-completion.result) as i32));
    }
    let raw = raw_from_result(completion.result).ok_or_else(|| {
      io::Error::new(
        io::ErrorKind::InvalidData,
        "backend completed with an invalid resource",
      )
    })?;
    Ok(Resource(Arc::new(InnerResource::new(raw, completion.resource_close()))))
  }

  /// Creates a Resource that borrows a file descriptor without taking ownership.
  ///
  /// The fd/handle will NOT be closed when this Resource is dropped. Use this when the
  /// fd is owned by external code (e.g., C FFI callers).
  ///
  /// # Safety
  ///
  /// The caller must ensure:
  /// - `fd` is a valid file descriptor
  /// - `fd` remains valid for the lifetime of this Resource and any clones
  // TODO: should this be unsafe?
  pub unsafe fn borrow(fd: Inner) -> Self {
    Resource(Arc::new(InnerResource::new_borrowed(fd)))
  }
}

#[cfg(windows)]
impl std::os::windows::io::FromRawHandle for Resource {
  unsafe fn from_raw_handle(handle: RawHandle) -> Self {
    Resource(Arc::new(InnerResource::new(
      handle,
      crate::backend::native::close,
    )))
  }
}

impl Resource {
  /// Returns a `Resource` for standard output (stdout).
  ///
  /// This creates a duplicate of the stdout file descriptor, so the returned
  /// `Resource` can be safely dropped without affecting the actual stdout.
  ///
  /// # Panics
  ///
  /// Panics if duplicating the file descriptor fails.
  ///
  /// # Examples
  ///
  /// ```rust,no_run
  /// use lio::api::resource::Resource;
  ///
  /// let stdout = Resource::stdout();
  /// // Use stdout for async I/O...
  /// ```
  pub fn stdout() -> Self {
    // SAFETY: the process std handles stay valid; borrowing them means this
    // `Resource` never closes them.
    unsafe { Self::borrow(crate::backend::native::stdout()) }
  }

  /// Returns a `Resource` for standard error (stderr).
  ///
  /// This creates a duplicate of the stderr file descriptor, so the returned
  /// `Resource` can be safely dropped without affecting the actual stderr.
  ///
  /// # Panics
  ///
  /// Panics if duplicating the file descriptor fails.
  ///
  /// # Examples
  ///
  /// ```rust,no_run
  /// use lio::api::resource::Resource;
  ///
  /// let stderr = Resource::stderr();
  /// // Use stderr for async I/O...
  /// ```
  pub fn stderr() -> Self {
    // SAFETY: the process std handles stay valid; borrowing them means this
    // `Resource` never closes them.
    unsafe { Self::borrow(crate::backend::native::stderr()) }
  }

  /// Returns a `Resource` for standard input (stdin).
  ///
  /// This creates a duplicate of the stdin file descriptor, so the returned
  /// `Resource` can be safely dropped without affecting the actual stdin.
  ///
  /// # Panics
  ///
  /// Panics if duplicating the file descriptor fails.
  ///
  /// # Examples
  ///
  /// ```rust,no_run
  /// use lio::api::resource::Resource;
  ///
  /// let stdin = Resource::stdin();
  /// // Use stdin for async I/O...
  /// ```
  pub fn stdin() -> Self {
    // SAFETY: the process std handles stay valid; borrowing them means this
    // `Resource` never closes them.
    unsafe { Self::borrow(crate::backend::native::stdin()) }
  }

  /// Returns a `Resource` representing the current working directory.
  ///
  /// This is a special resource that can be used with `openat` and similar
  /// operations to resolve paths relative to the current working directory.
  ///
  /// Unlike other resources, this does **not** represent a real file descriptor
  /// and will not be closed when dropped. It uses the special `AT_FDCWD` constant.
  ///
  /// # Examples
  ///
  /// ```rust,no_run
  /// use lio::api::resource::Resource;
  /// use std::ffi::CString;
  ///
  /// async fn open_file() -> std::io::Result<Resource> {
  ///     let cwd = Resource::cwd();
  ///     let path = CString::new("myfile.txt").unwrap();
  ///     lio::api::openat(&cwd, path, lio::api::OpenFlags::READ, 0).await
  /// }
  /// ```
  pub fn cwd() -> Self {
    // SAFETY: the cwd value is a sentinel the backends resolve (`AT_FDCWD` on
    // Unix); it is borrowed and therefore never closed.
    unsafe { Self::borrow(crate::backend::native::CWD) }
  }

  /// Returns the number of strong references to this resource.
  ///
  /// This counts how many `Resource` instances point to the same underlying
  /// file descriptor or handle. When the count reaches zero, the resource
  /// will be automatically closed.
  ///
  /// # Returns
  ///
  /// The number of `Resource` instances sharing this resource.
  ///
  /// # Examples
  ///
  /// ```
  /// use lio::api::resource::Resource;
  ///
  /// // Create a resource (using stdout as an example)
  /// let resource = Resource::stdout();
  /// assert_eq!(resource.count(), 1);
  ///
  /// let clone = resource.clone();
  /// assert_eq!(resource.count(), 2);
  /// assert_eq!(clone.count(), 2);
  ///
  /// drop(clone);
  /// assert_eq!(resource.count(), 1);
  /// ```
  pub fn count(&self) -> usize {
    Arc::strong_count(&self.0)
  }

  /// Checks if this is the last reference to the resource.
  ///
  /// When this returns `true`, dropping this `Resource` will cause the underlying
  /// file descriptor or handle to be closed. This is useful for determining when
  /// cleanup will occur.
  ///
  /// # Returns
  ///
  /// `true` if this is the only remaining reference (count == 1), meaning the
  /// resource will be closed when this `Resource` is dropped.
  ///
  /// # Examples
  ///
  /// ```
  /// use lio::api::resource::Resource;
  ///
  /// let resource = Resource::stdout();
  /// assert!(resource.will_close()); // Only reference
  ///
  /// let clone = resource.clone();
  /// assert!(!resource.will_close()); // Multiple references
  /// assert!(!clone.will_close());
  ///
  /// drop(clone);
  /// assert!(resource.will_close()); // Last reference again
  /// ```
  pub fn will_close(&self) -> bool {
    self.count() == 1
  }
}

impl std::fmt::Debug for Resource {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str("Resource")
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_stdout() {
    let stdout = Resource::stdout();
    assert_eq!(stdout.count(), 1);

    // Cloning should increase count
    let clone = stdout.clone();
    assert_eq!(stdout.count(), 2);
    assert_eq!(clone.count(), 2);

    // Dropping clone should decrease count
    drop(clone);
    assert_eq!(stdout.count(), 1);

    // Should be safe to drop (it's a dup'd handle)
    drop(stdout);
  }

  #[test]
  fn test_stderr() {
    let stderr = Resource::stderr();
    assert_eq!(stderr.count(), 1);

    let clone = stderr.clone();
    assert_eq!(stderr.count(), 2);

    drop(clone);
    assert_eq!(stderr.count(), 1);

    drop(stderr);
  }

  #[test]
  fn test_stdin() {
    let stdin = Resource::stdin();
    assert_eq!(stdin.count(), 1);

    let clone = stdin.clone();
    assert_eq!(stdin.count(), 2);

    drop(clone);
    assert_eq!(stdin.count(), 1);

    drop(stdin);
  }

  #[test]
  fn test_std_streams_are_independent() {
    // Each call should return an independent resource
    let stdout1 = Resource::stdout();
    let stdout2 = Resource::stdout();

    // They should be separate resources (different Arc instances)
    assert_eq!(stdout1.count(), 1);
    assert_eq!(stdout2.count(), 1);

    drop(stdout1);
    // stdout2 should still be valid
    assert_eq!(stdout2.count(), 1);
  }
}
