//! Unix domain socket support for lio.
//!
//! This module provides high-level abstractions for Unix domain sockets,
//! which allow for inter-process communication on the same machine.
//!
//! # Main Types
//!
//! - [`UnixListener`]: A Unix domain socket server
//! - [`UnixStream`]: A Unix domain socket connection
//!
//! # Examples
//!
//! ## Echo Server
//!
//! ```rust,no_run
//! use lio::net::unix::UnixListener;
//!
//! async fn echo_server() -> std::io::Result<()> {
//!     let listener = UnixListener::bind("/tmp/echo.sock")?;
//!
//!     loop {
//!         let stream = listener.accept().await?;
//!         let buffer = vec![0u8; 1024];
//!         let (result, buffer) = stream.recv(buffer).await;
//!         let bytes_read = result? as usize;
//!
//!         if bytes_read > 0 {
//!             let (result, _) = stream.send(buffer[..bytes_read].to_vec()).await;
//!             result?;
//!         }
//!     }
//! }
//! ```
//!
//! ## Client
//!
//! ```rust,no_run
//! use lio::net::unix::UnixStream;
//!
//! async fn client() -> std::io::Result<()> {
//!     let stream = UnixStream::connect("/tmp/echo.sock").await?;
//!
//!     let data = b"Hello, server!".to_vec();
//!     let (result, _) = stream.send(data).await;
//!     result?;
//!
//!     let buffer = vec![0u8; 1024];
//!     let (result, buffer) = stream.recv(buffer).await;
//!     let bytes_read = result? as usize;
//!     println!("Received: {:?}", &buffer[..bytes_read]);
//!
//!     Ok(())
//! }
//! ```

use std::io;
use std::os::fd::FromRawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use crate::api::{
  self,
  io::Io,
  ShutdownHow,
  ops::{Accept, Recv, Shutdown},
  resource::{AsResource, FromResource, IntoResource, Resource},
};

/// Creates a Unix socket and sets it to cloexec.
fn create_unix_socket() -> io::Result<i32> {
  let fd = syscall!(socket(libc::AF_UNIX, libc::SOCK_STREAM, 0))?;
  // Set CLOEXEC
  syscall!(fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC))?;
  Ok(fd)
}

/// Sets a socket to non-blocking mode.
fn set_nonblocking(fd: i32) -> io::Result<()> {
  let flags = syscall!(fcntl(fd, libc::F_GETFL))?;
  syscall!(fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK))?;
  Ok(())
}

/// A Unix domain socket server, listening for connections.
///
/// `UnixListener` provides a high-level interface for creating Unix domain socket
/// servers. After being created by binding to a path, it listens for and accepts
/// incoming connections.
///
/// # Examples
///
/// ```rust,no_run
/// use lio::net::unix::UnixListener;
///
/// async fn example() -> std::io::Result<()> {
///     let listener = UnixListener::bind("/tmp/my.sock")?;
///
///     loop {
///         let stream = listener.accept().await?;
///         println!("New connection!");
///     }
/// }
/// ```
pub struct UnixListener {
  socket: Resource,
}

impl AsResource for UnixListener {
  fn as_resource(&self) -> &Resource {
    &self.socket
  }
}

impl FromResource for UnixListener {
  fn from_resource(resource: Resource) -> Self {
    Self { socket: resource }
  }
}

impl UnixListener {
  /// Creates a new `UnixListener` bound to the specified path.
  ///
  /// The socket file will be created at the given path. If a file already exists
  /// at that path, this function will fail with `AddrInUse`.
  ///
  /// # Examples
  ///
  /// ```rust,no_run
  /// use lio::net::unix::UnixListener;
  ///
  /// fn example() -> std::io::Result<()> {
  ///     let listener = UnixListener::bind("/tmp/my.sock")?;
  ///     Ok(())
  /// }
  /// ```
  pub fn bind(path: impl AsRef<Path>) -> io::Result<Self> {
    let path = path.as_ref();

    // Create socket synchronously
    let fd = create_unix_socket()?;

    // Build sockaddr_un
    let (addr, addr_len) = sockaddr_un_from_path(path)?;

    // Bind
    // SAFETY: fd is a valid socket descriptor, addr is properly initialized
    let ret = unsafe {
      libc::bind(fd, &addr as *const _ as *const libc::sockaddr, addr_len)
    };
    if ret < 0 {
      // SAFETY: fd is a valid file descriptor we just created
      unsafe { libc::close(fd) };
      return Err(io::Error::last_os_error());
    }

    // Listen
    // SAFETY: fd is a valid bound socket descriptor
    let ret = unsafe { libc::listen(fd, 128) };
    if ret < 0 {
      // SAFETY: fd is a valid file descriptor
      unsafe { libc::close(fd) };
      return Err(io::Error::last_os_error());
    }

    // Set non-blocking for async accept
    set_nonblocking(fd)?;

    // SAFETY: fd is a valid file descriptor that we own
    let socket = unsafe { Resource::from_raw_fd(fd) };
    Ok(Self { socket })
  }

  /// Accepts a new incoming connection from this listener.
  ///
  /// This function will await until a new connection is established.
  ///
  /// # Examples
  ///
  /// ```rust,no_run
  /// use lio::net::unix::UnixListener;
  ///
  /// async fn example() -> std::io::Result<()> {
  ///     let listener = UnixListener::bind("/tmp/my.sock")?;
  ///
  ///     loop {
  ///         let stream = listener.accept().await?;
  ///         println!("New connection!");
  ///     }
  /// }
  /// ```
  pub fn accept(&self) -> Io<UnixAccept> {
    Io::from_op(UnixAccept::new(self.socket.clone()))
  }
}

/// A Unix domain socket connection.
///
/// `UnixStream` represents an established connection between two Unix domain
/// sockets. It can be created by connecting to a server or by accepting a
/// connection from a [`UnixListener`].
///
/// # Examples
///
/// ```rust,no_run
/// use lio::net::unix::UnixStream;
///
/// async fn example() -> std::io::Result<()> {
///     let stream = UnixStream::connect("/tmp/my.sock").await?;
///
///     let data = b"Hello!".to_vec();
///     let (result, _) = stream.send(data).await;
///     result?;
///
///     Ok(())
/// }
/// ```
pub struct UnixStream {
  socket: Resource,
}

impl IntoResource for UnixStream {
  fn into_resource(self) -> Resource {
    self.socket
  }
}

impl AsResource for UnixStream {
  fn as_resource(&self) -> &Resource {
    &self.socket
  }
}

impl FromResource for UnixStream {
  fn from_resource(resource: Resource) -> Self {
    Self { socket: resource }
  }
}

impl UnixStream {
  /// Connects to a Unix domain socket at the specified path.
  ///
  /// # Examples
  ///
  /// ```rust,no_run
  /// use lio::net::unix::UnixStream;
  ///
  /// async fn example() -> std::io::Result<()> {
  ///     let stream = UnixStream::connect("/tmp/my.sock").await?;
  ///     Ok(())
  /// }
  /// ```
  pub async fn connect(path: impl AsRef<Path>) -> io::Result<Self> {
    let path = path.as_ref();

    // Create socket
    let fd = create_unix_socket()?;
    set_nonblocking(fd)?;
    // SAFETY: fd is a valid file descriptor that we own
    let socket = unsafe { Resource::from_raw_fd(fd) };

    // Build sockaddr_un
    let (addr, addr_len) = sockaddr_un_from_path(path)?;

    // Convert to sockaddr_storage for the connect op
    // SAFETY: sockaddr_storage is safe to zero-initialize and is large enough to hold sockaddr_un
    let mut storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    // SAFETY: Both pointers are valid, non-overlapping, and size fits within sockaddr_storage
    unsafe {
      std::ptr::copy_nonoverlapping(
        &addr as *const _ as *const u8,
        &mut storage as *mut _ as *mut u8,
        addr_len as usize,
      );
    }

    // Connect using the low-level API
    api::connect_unix(&socket, storage, addr_len).await?;

    Ok(Self { socket })
  }

  /// Connects to a Unix domain socket synchronously (blocking).
  pub fn connect_sync(path: impl AsRef<Path>) -> io::Result<Self> {
    let path = path.as_ref();

    // Create socket (blocking)
    let fd = create_unix_socket()?;
    // SAFETY: fd is a valid file descriptor that we own
    let socket = unsafe { Resource::from_raw_fd(fd) };

    // Build sockaddr_un
    let (addr, addr_len) = sockaddr_un_from_path(path)?;

    // Connect synchronously
    // SAFETY: fd is a valid socket descriptor, addr is properly initialized
    let ret = unsafe {
      libc::connect(fd, &addr as *const _ as *const libc::sockaddr, addr_len)
    };
    if ret < 0 {
      return Err(io::Error::last_os_error());
    }

    // Set non-blocking for async I/O
    set_nonblocking(fd)?;

    Ok(Self { socket })
  }

  /// Receives data from the stream into the provided buffer.
  pub fn recv(&self, buf: Vec<u8>) -> Io<Recv<Vec<u8>>> {
    api::recv(&self.socket, buf, None)
  }

  /// Sends data through the stream.
  pub fn send(&self, buf: Vec<u8>) -> Io<crate::api::ops::Send<Vec<u8>>> {
    api::send(&self.socket, buf, None)
  }

  /// Shuts down the read, write, or both halves of this connection.
  pub fn shutdown(&self, how: ShutdownHow) -> Io<Shutdown> {
    api::shutdown(&self.socket, how)
  }
}

// ============================================================================
// Helper types and functions
// ============================================================================

/// TypedOp for accepting Unix domain socket connections.
pub struct UnixAccept {
  inner: Accept,
}

impl UnixAccept {
  pub(crate) fn new(socket: Resource) -> Self {
    Self { inner: Accept::new(socket) }
  }
}

impl crate::api::op::OpModel for UnixAccept {
  type Item = io::Result<UnixStream>;

  fn send_op(&mut self) -> crate::api::op::OpFlow {
    self.inner.send_op()
  }

  fn result(&mut self, res: isize) -> crate::api::op::StreamResult<Self::Item> {
    self
      .inner
      .result(res)
      .map(|res| res.map(|(socket, _addr)| UnixStream { socket }))
  }
}

/// Builds a `sockaddr_un` from a path.
fn sockaddr_un_from_path(
  path: &Path,
) -> io::Result<(libc::sockaddr_un, libc::socklen_t)> {
  let bytes = path.as_os_str().as_bytes();

  // sockaddr_un.sun_path is typically 104 bytes on BSD or 108 on Linux
  // We need to leave room for the null terminator
  let max_len = std::mem::size_of::<libc::sockaddr_un>()
    - std::mem::size_of::<libc::sa_family_t>()
    - 1; // -1 for null terminator

  if bytes.len() > max_len {
    return Err(io::Error::new(
      io::ErrorKind::InvalidInput,
      "path too long for Unix socket",
    ));
  }

  // SAFETY: sockaddr_un is a C struct safe to zero-initialize
  let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
  addr.sun_family = libc::AF_UNIX as libc::sa_family_t;

  // Copy path bytes
  // SAFETY: We checked that bytes.len() <= max_len above
  unsafe {
    std::ptr::copy_nonoverlapping(
      bytes.as_ptr(),
      addr.sun_path.as_mut_ptr() as *mut u8,
      bytes.len(),
    );
  }
  // Null terminator is already there from zeroed()

  // Calculate actual length
  let addr_len = std::mem::size_of::<libc::sa_family_t>() + bytes.len() + 1;

  Ok((addr, addr_len as libc::socklen_t))
}
