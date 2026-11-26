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
//!     let fd: Resource = lio::api::openat(
//!         &dir,
//!         path,
//!         lio::api::OpenFlags::READ,
//!         lio::api::FileMode::empty(),
//!     )
//!     .await?;
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
//!         lio::api::openat(&dir, path, lio::api::OpenFlags::READ, lio::api::FileMode::empty()).await?;
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
//! Or by adopting a descriptor the standard library already owns:
//!
//! ```rust
//! use std::fs::File;
//! use lio::api::resource::Resource;
//!
//! #[cfg(any(unix, target_os = "wasi"))]
//! fn from_file(file: File) -> Resource {
//!     use std::os::fd::OwnedFd;
//!     Resource::from(OwnedFd::from(file))
//! }
//!
//! #[cfg(windows)]
//! fn from_file(file: File) -> Resource {
//!     use std::os::windows::io::OwnedHandle;
//!     Resource::from(OwnedHandle::from(file))
//! }
//! ```

use std::io;
use std::sync::Arc;

macro_rules! impl_native_convervions {
  ($nice:ident) => {
    #[cfg(any(unix, target_os = "wasi"))]
    impl std::os::fd::AsFd for $nice {
      fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        // SAFETY: Owned guarrantees it's valid.
        assert!(
          crate::backend::native::is_os_descriptor(self.0.inner),
          "resource is not an open descriptor"
        );
        // SAFETY: public constructors only produce a non-negative descriptor
        // when it is owned and open (or a duplicate of one). Sentinels fail
        // the assert above.
        unsafe { std::os::fd::BorrowedFd::borrow_raw(self.0.inner) }
      }
    }

    #[cfg(any(unix, target_os = "wasi"))]
    impl std::os::fd::AsRawFd for $nice {
      fn as_raw_fd(&self) -> std::os::fd::RawFd {
        self.0.inner
      }
    }

    #[cfg(windows)]
    impl std::os::windows::io::AsHandle for $nice {
      fn as_handle(&self) -> std::os::windows::io::BorrowedHandle<'_> {
        assert!(
          crate::backend::native::is_os_descriptor(self.0.inner),
          "resource is not an open handle"
        );
        // SAFETY: as the Unix `AsFd` impl. The assert rejects null and the
        // cwd sentinel, which `BorrowedHandle` cannot represent.
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
#[cfg(any(unix, target_os = "wasi"))]
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
#[cfg(any(unix, target_os = "wasi"))]
fn raw_from_result(result: isize) -> Option<RawResource> {
  RawResource::try_from(result).ok().filter(|fd| *fd >= 0)
}

/// Converts a successful completion result into the handle it carries,
/// rejecting the null and pseudo-handle range.
#[cfg(windows)]
fn raw_from_result(result: isize) -> Option<RawResource> {
  (result > 0).then_some(result as RawResource)
}

/// How an owned [`Resource`] is closed.
///
/// Backends return one from
/// [`IoBackend::resource_close`](crate::backend::IoBackend::resource_close).
/// lio invokes it when the last clone of a resource that backend produced is
/// dropped. The function itself is not callable from outside this crate:
/// closing an arbitrary raw descriptor is not a safe operation.
#[derive(Clone, Copy)]
pub struct ResourceClose {
  close: fn(RawResource),
}

/// Closes one native descriptor/handle.
///
/// # Safety
///
/// Only as [`ResourceClose::native`]: the argument is an owned descriptor
/// closed exactly once.
fn noop_close(_raw: RawResource) {}

fn close_owned(raw: RawResource) {
  // SAFETY: the caller is `ResourceClose::call`, which runs for an owned
  // resource exactly once.
  unsafe { crate::backend::native::close(raw) }
}

impl ResourceClose {
  /// Closes nothing.
  ///
  /// A simulation uses this. The descriptor is not an OS resource.
  pub const fn noop() -> Self {
    Self { close: noop_close }
  }

  /// Closes native OS descriptors/handles.
  pub const fn native() -> Self {
    Self { close: close_owned }
  }

  /// Closes resources with `close`, for a backend whose resources are not
  /// native descriptors.
  #[cfg_attr(not(target_os = "wasi"), allow(dead_code))]
  pub(crate) const fn with(close: fn(RawResource)) -> Self {
    Self { close }
  }

  pub(crate) fn call(self, raw: RawResource) {
    (self.close)(raw)
  }
}

impl PartialEq for ResourceClose {
  fn eq(&self, other: &Self) -> bool {
    std::ptr::fn_addr_eq(self.close, other.close)
  }
}

impl Eq for ResourceClose {}

impl std::fmt::Debug for ResourceClose {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str("ResourceClose")
  }
}

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
  close: Option<ResourceClose>,
}

impl InnerResource {
  /// Creates a new owned resource that `close` releases on drop.
  fn new(inner: Inner, close: ResourceClose) -> Self {
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
      // `close` is the closer paired with this owned descriptor at creation.
      close.call(self.inner);
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

#[cfg(any(unix, target_os = "wasi"))]
impl From<std::os::fd::OwnedFd> for Resource {
  fn from(fd: std::os::fd::OwnedFd) -> Self {
    use std::os::fd::IntoRawFd;
    Self::from_owned(fd.into_raw_fd())
  }
}

#[cfg(windows)]
impl From<std::os::windows::io::OwnedHandle> for Resource {
  fn from(handle: std::os::windows::io::OwnedHandle) -> Self {
    use std::os::windows::io::IntoRawHandle;
    Self::from_owned(handle.into_raw_handle())
  }
}

#[cfg(windows)]
impl From<std::os::windows::io::OwnedSocket> for Resource {
  fn from(socket: std::os::windows::io::OwnedSocket) -> Self {
    use std::os::windows::io::IntoRawSocket;
    Self::from_owned(socket.into_raw_socket() as RawHandle)
  }
}

impl Resource {
  fn from_owned(raw: RawResource) -> Self {
    Resource(Arc::new(InnerResource::new(raw, ResourceClose::native())))
  }

  /// Borrows a process-owned descriptor. Dropping it does not close `raw`.
  fn borrow_stdio(raw: RawResource) -> Self {
    Resource(Arc::new(InnerResource::new_borrowed(raw)))
  }
  /// Takes ownership of the descriptor/handle a resource-creating
  /// operation completed with.
  ///
  /// The resource is closed with the close function of the backend that
  /// produced the completion.
  ///
  /// # Safety
  ///
  /// A non-negative `completion.result()` must carry a newly created
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
    if completion.result() < 0 {
      return Err(completion.os_error());
    }
    let raw = raw_from_result(completion.result()).ok_or_else(|| {
      io::Error::new(
        io::ErrorKind::InvalidData,
        "backend completed with an invalid resource",
      )
    })?;
    Ok(Resource(Arc::new(InnerResource::new(raw, completion.resource_close()))))
  }

  /// Borrows `raw` without closing it on drop.
  ///
  /// # Safety
  ///
  /// `raw` is either a sentinel this crate understands ([`cwd`](Self::cwd),
  /// [`invalid`](Self::invalid)) or stays open for every use of the returned
  /// resource and its clones. It must not be an owned descriptor: nothing
  /// closes it.
  pub(crate) unsafe fn borrow_raw(raw: RawResource) -> Self {
    Resource(Arc::new(InnerResource::new_borrowed(raw)))
  }

  /// A resource that is not an open descriptor.
  ///
  /// Dropping it closes nothing. Submitting I/O on it fails with a bad-handle
  /// error. [`std::os::fd::AsFd`] (and the Windows handle equivalent) panics:
  /// there is no open descriptor to borrow.
  pub fn invalid() -> Self {
    // SAFETY: `INVALID` is never open and is never closed.
    unsafe { Self::borrow_raw(crate::backend::native::INVALID) }
  }

  /// Duplicates an open descriptor so the copy is owned by the result.
  ///
  /// # Errors
  ///
  /// The descriptor cannot be duplicated, including sentinels from
  /// [`cwd`](Self::cwd) and [`invalid`](Self::invalid).
  #[cfg(any(unix, target_os = "wasi"))]
  pub fn try_duplicate(&self) -> io::Result<Self> {
    crate::backend::native::duplicate(self.0.inner).map(Self::from_owned)
  }

  /// Duplicates an open handle so the copy is owned by the result.
  ///
  /// # Errors
  ///
  /// The handle cannot be duplicated, including sentinels from
  /// [`cwd`](Self::cwd) and [`invalid`](Self::invalid).
  #[cfg(windows)]
  pub fn try_duplicate(&self) -> io::Result<Self> {
    crate::backend::native::duplicate(self.0.inner).map(Self::from_owned)
  }

  /// Duplicates a borrowed descriptor. The original stays open; the result
  /// owns the copy and closes it on drop.
  ///
  /// # Errors
  ///
  /// As [`try_duplicate`](Self::try_duplicate).
  #[cfg(any(unix, target_os = "wasi"))]
  pub fn duplicate_of(fd: impl std::os::fd::AsFd) -> io::Result<Self> {
    use std::os::fd::AsRawFd;
    crate::backend::native::duplicate(fd.as_fd().as_raw_fd())
      .map(Self::from_owned)
  }

  /// Duplicates a borrowed handle. The original stays open; the result owns
  /// the copy and closes it on drop.
  ///
  /// # Errors
  ///
  /// As [`try_duplicate`](Self::try_duplicate).
  #[cfg(windows)]
  pub fn duplicate_of(
    handle: impl std::os::windows::io::AsHandle,
  ) -> io::Result<Self> {
    use std::os::windows::io::AsRawHandle;
    crate::backend::native::duplicate(handle.as_handle().as_raw_handle())
      .map(Self::from_owned)
  }
}

impl Resource {
  /// Borrows the process standard output.
  ///
  /// Dropping the returned resource does not close the process stdout.
  /// [`try_duplicate`](Self::try_duplicate) returns an owned descriptor that
  /// is closed on drop.
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
    Self::borrow_stdio(crate::backend::native::stdout())
  }

  /// Borrows the process standard error.
  ///
  /// Dropping the returned resource does not close the process stderr.
  /// [`try_duplicate`](Self::try_duplicate) returns an owned descriptor that
  /// is closed on drop.
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
    Self::borrow_stdio(crate::backend::native::stderr())
  }

  /// Borrows the process standard input.
  ///
  /// Dropping the returned resource does not close the process stdin.
  /// [`try_duplicate`](Self::try_duplicate) returns an owned descriptor that
  /// is closed on drop.
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
    Self::borrow_stdio(crate::backend::native::stdin())
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
  ///     lio::api::openat(
  ///         &cwd,
  ///         path,
  ///         lio::api::OpenFlags::READ,
  ///         lio::api::FileMode::empty(),
  ///     )
  ///     .await
  /// }
  /// ```
  pub fn cwd() -> Self {
    // SAFETY: the cwd value is a sentinel the backends resolve (`AT_FDCWD` on
    // Unix); it is borrowed and therefore never closed. `AsFd`/`AsHandle`
    // panic on it instead of borrowing a non-descriptor.
    unsafe { Self::borrow_raw(crate::backend::native::CWD) }
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

  /// Whether dropping this value closes the underlying descriptor.
  ///
  /// That is true only for the last clone of an owned resource. A borrowed
  /// resource never closes: stdout, stdin, stderr, [`cwd`](Self::cwd), and
  /// [`invalid`](Self::invalid).
  ///
  /// # Examples
  ///
  /// ```
  /// use lio::api::resource::Resource;
  ///
  /// let stdout = Resource::stdout();
  /// assert!(!stdout.will_close());
  ///
  /// let clone = stdout.clone();
  /// assert!(!stdout.will_close());
  /// assert!(!clone.will_close());
  /// ```
  pub fn will_close(&self) -> bool {
    self.0.close.is_some() && self.count() == 1
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

    // Borrowed: dropping it must not close the process stdout.
    drop(stdout);
    assert!(!Resource::stdout().will_close());
    assert!(!Resource::stdin().will_close());
    assert!(!Resource::stderr().will_close());
    assert!(!Resource::cwd().will_close());
    assert!(!Resource::invalid().will_close());
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
  fn dropping_stdout_leaves_the_process_stream_open() {
    drop(Resource::stdout());
    assert!(Resource::stdout().try_duplicate().is_ok());
  }

  #[test]
  fn invalid_resource_is_not_a_borrowable_descriptor() {
    let invalid = Resource::invalid();
    #[cfg(any(unix, target_os = "wasi"))]
    {
      use std::os::fd::AsRawFd;
      assert_eq!(invalid.as_raw_fd(), -1);
    }
    let panicked = std::panic::catch_unwind(|| {
      #[cfg(any(unix, target_os = "wasi"))]
      {
        use std::os::fd::AsFd;
        let _ = invalid.as_fd();
      }
      #[cfg(windows)]
      {
        use std::os::windows::io::AsHandle;
        let _ = invalid.as_handle();
      }
    });
    assert!(panicked.is_err());
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
