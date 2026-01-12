//! Resource management for file descriptors and handles.
//!
//! This module provides the [`Resource`] type, which is lio's abstraction around
//! platform-specific I/O resources (file descriptors on Unix and some other platforms, handles on Windows).
//!
//! ```rust
//! use std::ffi::CString;
//! use lio::api::resource::Resource;
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
//! Cloning [`Resource`]'s is very cheap, all clones point to the same
//! underlying file descriptor/handle:
//!
//! ```rust
//! use lio::api::resource::Resource;
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
//! use lio::api::resource::Resource;
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
//! use lio::api::resource::Resource;
//!
//! # lio::init();
//! fn from_raw(raw_fd: RawFd) -> Resource {
//!     // Safe if raw_fd is a valid, open file descriptor
//!     unsafe { Resource::from_raw_fd(raw_fd) }
//! }
//! # lio::exit();
//! ```

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
  // /// Whether to automatically close this resource on drop (not yet implemented)
  // should_close: AtomicBool,
}

impl Owned {
  /// Creates a new owned resource.
  ///
  /// By default, `should_close` is false, meaning the resource will NOT be automatically
  /// closed on drop. This must be explicitly enabled via `Resource::close()`.
  fn new(inner: Inner) -> Self {
    Self { inner /*should_close: AtomicBool::new(true)*/ }
  }
}

impl Drop for Owned {
  /// Drops the owned resource, potentially closing it automatically.
  ///
  /// If `should_close` is true, this will close the underlying resource. Currently
  /// unimplemented and will panic if enabled.
  fn drop(&mut self) {
    // if self.should_close.load(Ordering::Acquire) {
    let _ = syscall!(close(self.inner));
    // let op = api::close(UniqueResource(Owned {
    //   inner: self.inner,
    //   should_close: AtomicBool::new(true),
    // }));
    //
    // // NOTE: Is this really neccessary?
    // let _ = op.blocking();
    // }
  }
}

/// A reference-counted, platform-independent wrapper around OS I/O resources.
///
/// See the [module documentation](self) for usage examples and details.
#[derive(Clone)]
pub struct Resource(Arc<Owned>);

/// Trait for types that can be converted into a [`Resource`].
///
/// This trait is implemented by high-level types like [`Socket`](crate::net::Socket),
/// [`TcpSocket`](crate::net::TcpSocket), and [`TcpListener`](crate::net::TcpListener)
/// to allow conversion to the underlying resource.
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
/// let socket = Socket::new(libc::AF_INET, libc::SOCK_STREAM, 0).await?;
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
/// let socket = Socket::new(libc::AF_INET, libc::SOCK_STREAM, 0).await?;
/// inspect_resource(&socket);
/// // socket is still usable here
/// ```
pub trait AsResource {
  /// Returns a reference to the underlying [`Resource`].
  ///
  /// This borrows the resource without transferring ownership.
  fn as_resource(&self) -> &Resource;
}

/// Blanket implementation for references to types implementing [`AsResource`].
///
/// This allows passing `&T` where `T: AsResource` to functions that accept
/// `impl AsResource`, avoiding the need to explicitly call `.as_resource()`.
impl<R> AsResource for &R
where
  R: AsResource,
{
  fn as_resource(&self) -> &Resource {
    (*self).as_resource()
  }
}

/// Direct implementation for [`Resource`] itself.
///
/// This allows using a `Resource` directly where `AsResource` is expected.
impl AsResource for Resource {
  fn as_resource(&self) -> &Resource {
    self
  }
}

/// Trait for types that can be constructed from a [`Resource`].
///
/// This trait is implemented by high-level types like [`Socket`](crate::net::Socket),
/// [`TcpSocket`](crate::net::TcpSocket), and [`TcpListener`](crate::net::TcpListener)
/// to allow wrapping a raw [`Resource`] in a typed interface.
///
/// # Safety Contract
///
/// Implementors guarantee that the [`Resource`] is compatible with the type being
/// constructed. For example, constructing a [`TcpSocket`](crate::net::TcpSocket)
/// from a resource requires that the resource is actually a TCP socket file descriptor.
///
/// # Examples
///
/// ```rust,ignore
/// use lio::api::resource::{Resource, FromResource};
/// use lio::net::Socket;
///
/// // Assuming we have a raw resource that's a socket
/// let raw_resource: Resource = /* ... */;
///
/// // Wrap it in a Socket
/// let socket = Socket::from_resource(raw_resource);
/// ```
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
  /// ```rust,ignore
  /// use lio::api::resource::Resource;
  ///
  /// let resource: Resource = /* ... */;
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
  /// ```rust,ignore
  /// use lio::api::resource::Resource;
  ///
  /// let resource: Resource = /* ... */;
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
