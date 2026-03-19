//! lio's low-level I/O API.
//!
//! This module contains all the syscall-oriented I/O operations provided by lio.
//! Each function maps directly to a system call and returns a [`Io`] handle
//! representing the in-flight operation.
//!
//! # Design Philosophy
//!
//! The API is designed to be:
//! - **Explicit**: Direct syscall mapping with minimal abstraction.
//! - **Zero-copy**: Buffers are moved into operations and returned on completions.
//! - **Flexible**: Multiple completion modes (async, blocking, channels, callbacks).
//! - **Type-safe**: Resources are reference-counted and automatically cleaned up.
//!
//! ## Synchronisation
//!
//! Lio is compatible with any synchronisation method, through the [`Io`] type:
//! - [async](io::IoFuture) (runtime-independent).
//! - [Callbacks](io::Io::when_done).
//! - Channels: [get receiver](io::Io::send) *or* [send with your own](io::Io::send_with).
//!
//! # Buffer Ownership
//!
//! Operations that use buffers ([`read`], [`write`](write()), [`recv`], [`send`]) take
//! ownership of the buffer and return it along with the result. This enables
//! zero-copy I/O while ensuring memory safety:
//!
//! ```no_run
//! use lio::{Lio, api};
//!
//! let mut lio = Lio::new(64).unwrap();
//! let fd = api::resource::Resource::stdin();
//! let buf = vec![0u8; 1024];
//! let (result, buf) = api::read(&fd, buf).with_lio(&mut lio).wait();
//! // buf is returned and can be reused
//! ```
//!
//! # Example: Echo Server (Conceptual)
//!
//! This shows the general pattern for building a server with lio. Note that
//! in practice you'd use the higher-level `net` module (requires `high` feature).
//!
//! ```no_run
//! use lio::{Lio, api};
//!
//! fn echo_server(lio: &mut Lio) -> std::io::Result<()> {
//!     // Create and bind socket
//!     let sock = api::socket(libc::AF_INET, libc::SOCK_STREAM, 0)
//!         .with_lio(lio).wait()?;
//!     let addr = "127.0.0.1:8080".parse().unwrap();
//!     api::bind(&sock, addr).with_lio(lio).wait()?;
//!     api::listen(&sock, 128).with_lio(lio).wait()?;
//!
//!     // Accept one connection for this example
//!     let (client, _addr) = api::accept(&sock).with_lio(lio).wait()?;
//!
//!     // Echo once
//!     let buf = vec![0u8; 1024];
//!     let (result, buf) = api::recv(&client, buf, None).with_lio(lio).wait();
//!     let n = result? as usize;
//!     if n > 0 {
//!         let _ = api::send(&client, buf[..n].to_vec(), None).with_lio(lio).wait();
//!     }
//!     Ok(())
//! }
//! ```
//!
//! # See Also
//!
//! - [`Io`] - Operation handle with multiple completion modes
//! - [`Resource`](crate::api::resource::Resource) - Reference-counted file descriptor wrapper
//! - [`crate::buf`] - Buffer types and pooling

pub mod io;
pub mod op;
pub mod ops;
pub mod resource;

use crate::api::resource::AsResource;
use crate::buf::{IoBuf, IoBufMut, IoBufMutVec, IoBufVec};
use io::Io;
use std::{ffi::CString, net::SocketAddr, path::Path, time::Duration};

#[cfg(unix)]
use std::os::fd::RawFd;
#[cfg(windows)]
use std::os::windows::io::RawHandle;

doc_op! {
    short: "A no-op that completes immediately. Useful for waking up the event loop or testing.",

    pub fn nop() -> Io<ops::Nop> {
        Io::from_op(ops::Nop)
    }
}

doc_op! {
    short: "Closes a raw file descriptor.",
    syscall: "close(2)",

    #[cfg(unix)]
    pub fn close(fd: RawFd) -> Io<ops::Close> {
        Io::from_op(ops::Close::new(fd))
    }
}

/// Closes a raw handle.
///
/// # Parameters
/// - `handle`: The raw handle to close
/// - `is_socket`: Whether this handle is a socket (uses closesocket()) or a file (uses CloseHandle())
#[cfg(windows)]
pub fn close(handle: RawHandle, is_socket: bool) -> Io<ops::Close> {
  Io::from_op(ops::Close::new(handle, is_socket))
}

doc_op! {
    short: "Shuts down the read, write, or both halves of a socket connection.",
    syscall: "shutdown(2)",
    doc_link: "https://man7.org/linux/man-pages/man2/shutdown.2.html",

    ///
    /// # Parameters
    ///
    /// - `res`: The socket resource to shut down
    /// - `how`: Specifies what to shut down:
    ///   - `libc::SHUT_RD` (0): Stop receiving data
    ///   - `libc::SHUT_WR` (1): Stop sending data
    ///   - `libc::SHUT_RDWR` (2): Stop both receiving and sending
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// use lio::api::resource::Resource;
    ///
    /// async fn shutdown_example() -> std::io::Result<()> {
    ///     # let socket = Resource::stdin(); // Placeholder for a real socket
    ///     // Stop sending data on this socket
    ///     lio::api::shutdown(&socket, libc::SHUT_WR).await?;
    ///     Ok(())
    /// }
    /// ```
    pub fn shutdown(res: &impl AsResource, how: i32) -> Io<ops::Shutdown> {
        Io::from_op(ops::Shutdown::new(res.as_resource().clone(), how))
    }
}

doc_op! {
    short: "Sleeps for the specified duration.",

    pub fn sleep(duration: Duration) -> Io<ops::Sleep> {
        Io::from_op(ops::Sleep::new(duration))
    }
}

/// Wraps an operation with a timeout deadline.
///
/// If the timeout fires before the inner operation completes, the inner operation
/// is cancelled and `Err(TimedOut)` is returned.
///
/// If the inner operation completes before the timeout, the timeout is cancelled
/// and the inner operation's result is returned.
///
/// # Example
///
/// ```rust,no_run
/// use lio::api;
/// use std::time::Duration;
///
/// async fn timeout_example() -> Result<(), api::ops::TimedOut> {
///     # use lio::api::resource::Resource;
///     # let socket = Resource::stdin();
///     let buf = vec![0u8; 1024];
///     // Wait up to 5 seconds for data
///     let (result, buf) = api::timeout(
///         Duration::from_secs(5),
///         api::recv(&socket, buf, None),
///     ).await?;
///     // result is io::Result<isize> from the recv
///     Ok(())
/// }
/// ```
///
/// # Platform Support
///
/// - **Linux (io_uring)**: Uses `IORING_OP_LINK_TIMEOUT` for efficient kernel-native
///   timeout handling.
/// - **Other Unix (pollingv2)**: Uses userspace timeout coordination via TimeManager.
#[cfg(unix)]
#[cfg_attr(docsrs, doc(cfg(unix)))]
pub fn timeout<T: op::TypedOp>(
  duration: Duration,
  io: Io<T>,
) -> Io<ops::Timeout<T>> {
  Io::from_op(ops::Timeout::new(io.into_inner(), duration))
}

/// Watches a file or directory for changes.
///
/// This is a one-shot operation: it waits until the file changes in any of the
/// ways specified by `mask`, then returns what actually happened.
///
/// # Example
///
/// ```rust,no_run
/// use lio::api;
/// use lio::api::ops::WatchMask;
/// use std::path::Path;
///
/// async fn watch_example() -> std::io::Result<()> {
///     // Watch for modifications or deletions
///     let events = api::watch(
///         "/tmp/myfile.txt",
///         WatchMask::MODIFY | WatchMask::DELETE,
///     ).await?;
///
///     if events.contains(WatchMask::MODIFY) {
///         println!("File was modified!");
///     }
///     if events.contains(WatchMask::DELETE) {
///         println!("File was deleted!");
///     }
///     Ok(())
/// }
/// ```
///
/// # Platform Support
///
/// - **Linux**: Uses inotify
/// - **BSD/macOS**: Uses kqueue with EVFILT_VNODE
#[cfg(unix)]
#[cfg_attr(docsrs, doc(cfg(unix)))]
pub fn watch(path: impl AsRef<Path>, mask: ops::WatchMask) -> Io<ops::Watch> {
  let path_cstring = CString::new(path.as_ref().as_os_str().as_encoded_bytes())
    .expect("path contains null byte");
  Io::from_op(ops::Watch::new(path_cstring, mask))
}

/// Creates a streaming watch operation that yields multiple file change events.
///
/// Unlike [`watch`] which completes after a single event, `watch_stream` continues
/// watching the file and yields events as they occur.
///
/// # Example
///
/// ```rust,no_run
/// use lio::{Lio, api};
/// use lio::api::ops::WatchMask;
///
/// async fn watch_file(lio: &Lio) -> std::io::Result<()> {
///     let mut stream = api::watch_stream(
///         "/tmp/myfile.txt",
///         WatchMask::MODIFY | WatchMask::DELETE,
///     ).with_lio(lio);
///
///     while let Some(result) = stream.next().await {
///         let events = result?;
///         if events.contains(WatchMask::MODIFY) {
///             println!("File was modified!");
///         }
///         if events.contains(WatchMask::DELETE) {
///             println!("File was deleted!");
///             break;
///         }
///     }
///     Ok(())
/// }
/// ```
///
/// # Comparison with [`watch`]
///
/// - `watch()` waits for a single event and completes
/// - `watch_stream()` continues watching and yields events until closed
///
/// # Platform Support
///
/// - **Linux**: Uses inotify with epoll/io_uring for async notification
/// - **BSD/macOS**: Uses kqueue with EVFILT_VNODE
#[cfg(unix)]
#[cfg_attr(docsrs, doc(cfg(unix)))]
pub fn watch_stream(
  path: impl AsRef<Path>,
  mask: ops::WatchMask,
) -> io::IoStreamBuilder<ops::WatchStream> {
  let path_cstring = CString::new(path.as_ref().as_os_str().as_encoded_bytes())
    .expect("path contains null byte");
  io::IoStreamBuilder::from_op(ops::WatchStream::new(path_cstring, mask))
}

doc_op!(
  short: "Create a symlink.",
  syscall: "symlinkat(2)",
  doc_link: "https://man7.org/linux/man-pages/man2/symlink.2.html",

  pub fn symlinkat(dir_res: &impl AsResource, target: CString, linkpath: CString) -> Io<ops::SymlinkAt> {
    Io::from_op(ops::SymlinkAt::new(dir_res.as_resource().clone(), target, linkpath))
  }
);

doc_op!(
  short: "Create a hard-link.",
  syscall: "linkat(2)",
  doc_link: "https://man7.org/linux/man-pages/man2/linkat.2.html",

  pub fn linkat(old_dir_res: &impl AsResource, old_path: CString, new_dir_res: impl AsResource, linkpath: CString) -> Io<ops::LinkAt> {
    Io::from_op(ops::LinkAt::new(old_dir_res.as_resource().clone(), old_path, new_dir_res.as_resource().clone(), linkpath))
  }
);

doc_op!(
  short: "Remove a file or directory.",
  syscall: "unlinkat(2)",
  doc_link: "https://man7.org/linux/man-pages/man2/unlinkat.2.html",

  ///
  /// # Parameters
  ///
  /// - `dir_res`: Directory file descriptor (use `AT_FDCWD` for current directory)
  /// - `path`: Path to the file or directory to remove
  /// - `flags`: Optional flags. Use `libc::AT_REMOVEDIR` to remove a directory.
  ///
  /// # Examples
  ///
  /// ```rust,no_run
  /// use std::ffi::CString;
  ///
  /// async fn unlink_example() -> std::io::Result<()> {
  ///     # use lio::api::resource::Resource;
  ///     # use std::os::fd::FromRawFd;
  ///     # let dir = unsafe { Resource::from_raw_fd(libc::AT_FDCWD) };
  ///     let path = CString::new("/tmp/file.txt").unwrap();
  ///     lio::api::unlinkat(&dir, path, 0).await?;
  ///     Ok(())
  /// }
  /// ```
  pub fn unlinkat(dir_res: &impl AsResource, path: CString, flags: i32) -> Io<ops::UnlinkAt> {
    Io::from_op(ops::UnlinkAt::new(dir_res.as_resource().clone(), path, flags))
  }
);

doc_op!(
  short: "Rename a file or directory.",
  syscall: "renameat(2)",
  doc_link: "https://man7.org/linux/man-pages/man2/renameat.2.html",

  ///
  /// # Parameters
  ///
  /// - `old_dir_res`: Directory fd for the old path
  /// - `old_path`: Current path of the file
  /// - `new_dir_res`: Directory fd for the new path
  /// - `new_path`: New path for the file
  ///
  /// # Examples
  ///
  /// ```rust,no_run
  /// use std::ffi::CString;
  ///
  /// async fn rename_example() -> std::io::Result<()> {
  ///     # use lio::api::resource::Resource;
  ///     # use std::os::fd::FromRawFd;
  ///     # let dir = unsafe { Resource::from_raw_fd(libc::AT_FDCWD) };
  ///     let old = CString::new("/tmp/old.txt").unwrap();
  ///     let new = CString::new("/tmp/new.txt").unwrap();
  ///     lio::api::renameat(&dir, old, &dir, new).await?;
  ///     Ok(())
  /// }
  /// ```
  pub fn renameat(old_dir_res: &impl AsResource, old_path: CString, new_dir_res: &impl AsResource, new_path: CString) -> Io<ops::RenameAt> {
    Io::from_op(ops::RenameAt::new(old_dir_res.as_resource().clone(), old_path, new_dir_res.as_resource().clone(), new_path))
  }
);

doc_op!(
  short: "Create a directory.",
  syscall: "mkdirat(2)",
  doc_link: "https://man7.org/linux/man-pages/man2/mkdirat.2.html",

  ///
  /// # Parameters
  ///
  /// - `dir_res`: Directory file descriptor (use `AT_FDCWD` for current directory)
  /// - `path`: Path to the new directory
  /// - `mode`: Permission bits for the new directory (e.g., `0o755`)
  ///
  /// # Examples
  ///
  /// ```rust,no_run
  /// use std::ffi::CString;
  ///
  /// async fn mkdir_example() -> std::io::Result<()> {
  ///     # use lio::api::resource::Resource;
  ///     # use std::os::fd::FromRawFd;
  ///     # let dir = unsafe { Resource::from_raw_fd(libc::AT_FDCWD) };
  ///     let path = CString::new("/tmp/new_dir").unwrap();
  ///     lio::api::mkdirat(&dir, path, 0o755).await?;
  ///     Ok(())
  /// }
  /// ```
  pub fn mkdirat(dir_res: &impl AsResource, path: CString, mode: u32) -> Io<ops::MkdirAt> {
    Io::from_op(ops::MkdirAt::new(dir_res.as_resource().clone(), path, mode))
  }
);

doc_op! {
    short: "Synchronizes file data to storage.",
    syscall: "fsync(2)",

    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// async fn write_example() -> std::io::Result<()> {
    ///     # use lio::api::resource::Resource;
    ///     # let fd = Resource::stdin();
    ///     lio::api::fsync(&fd).await?;
    ///     Ok(())
    /// }
    /// ```
    pub fn fsync(res: &impl AsResource) -> Io<ops::Fsync> {
        Io::from_op(ops::Fsync::new(res.as_resource().clone()))
    }
}

doc_op! {
    short: "Writes data from buffer to file descriptor.",
    syscall: "write(2)",
    doc_link: "https://man7.org/linux/man-pages/man2/write.2.html",

    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// async fn write_example() -> std::io::Result<()> {
    ///     # use lio::api::resource::Resource;
    ///     # let fd = Resource::stdin();
    ///     let data = b"Hello, World!".to_vec();
    ///     let (result_bytes_written, _buf) = lio::api::write(&fd, data).await;
    ///     println!("Wrote {} bytes", result_bytes_written?);
    ///     Ok(())
    /// }
    /// ```
    pub fn write<B>(res: &impl AsResource, buf: B) -> Io<ops::WriteV<B>>
    where
        B: IoBuf
    {
        Io::from_op(ops::WriteV::new(res.as_resource().clone(), buf))
    }
}

doc_op! {
    short: "Writes data from buffer to file descriptor at a specific offset.",
    syscall: "pwrite(2)",
    doc_link: "https://man7.org/linux/man-pages/man2/pwrite.2.html",

    ///
    /// # Errors
    ///
    /// - `EINVAL`: Invalid offset on non-seekable fd.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// async fn write_example() -> std::io::Result<()> {
    ///     # use lio::api::resource::Resource;
    ///     # let fd = Resource::stdin();
    ///     let data = b"Hello, World!".to_vec();
    ///     let (result_bytes_written, _buf) = lio::api::write_at(&fd, data, 0).await;
    ///     println!("Wrote {} bytes", result_bytes_written?);
    ///     Ok(())
    /// }
    /// ```
    pub fn write_at<B>(res: &impl AsResource, buf: B, offset: i64) -> Io<ops::WriteVAt<B>>
    where
        B: IoBuf
    {
        Io::from_op(ops::WriteVAt::new(res.as_resource().clone(), buf, offset))
    }
}

doc_op! {
    short: "Reads resource into provided buffer.",
    syscall: "read(2)",
    doc_link: "https://man7.org/linux/man-pages/man2/read.2.html",

    pub fn read<B>(res: &impl AsResource, mem: B) -> Io<ops::ReadV<B>>
    where
        B: IoBufMut
    {
        Io::from_op(ops::ReadV::new(res.as_resource().clone(), mem))
    }
}

doc_op! {
  short: "Reads resource into provided buffer with a offset.",
  syscall: "pread(2)",
  doc_link: "https://man7.org/linux/man-pages/man2/pwrite.2.html",

  pub fn read_at<B>(res: &impl AsResource, mem: B, offset: i64) -> Io<ops::ReadVAt<B>>
  where
      B: IoBufMut
  {
    Io::from_op(ops::ReadVAt::new(res.as_resource().clone(), mem, offset))
  }
}

doc_op! {
    short: "Truncates a file to a specified length.",
    syscall: "ftruncate(2)",

    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// async fn truncate_example() -> std::io::Result<()> {
    ///     # use lio::api::resource::Resource;
    ///     # let fd = Resource::stdin();
    ///     lio::api::truncate(&fd, 1024).await?; // Truncate to 1KB
    ///     Ok(())
    /// }
    /// ```
    pub fn truncate(res: &impl AsResource, len: u64) -> Io<ops::Truncate> {
        Io::from_op(ops::Truncate::new(res.as_resource().clone(), len))
    }
}

doc_op! {
    short: "Creates a new socket with the specified domain, type, and protocol.",
    syscall: "socket(2)",

    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// async fn socket_example() -> std::io::Result<()> {
    ///     // AF_INET (IPv4), SOCK_STREAM (TCP), protocol 0 (default)
    ///     let sock = lio::api::socket(libc::AF_INET, libc::SOCK_STREAM, 0).await?;
    ///     println!("Created socket: {:?}", sock);
    ///     Ok(())
    /// }
    /// ```
    pub fn socket(domain: libc::c_int, ty: libc::c_int, proto: libc::c_int) -> Io<ops::Socket> {
        Io::from_op(ops::Socket::new(domain, ty, proto))
    }
}

doc_op! {
    short: "Binds a socket to a specific address.",
    syscall: "bind(2)",

    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// async fn bind_example() -> std::io::Result<()> {
    ///     # use lio::api::resource::Resource;
    ///     # let fd = Resource::stdin();
    ///     let addr = "127.0.0.1:8080".parse::<std::net::SocketAddr>().unwrap();
    ///     lio::api::bind(&fd, addr).await?;
    ///     Ok(())
    /// }
    /// ```
    pub fn bind(resource: &impl AsResource, addr: SocketAddr) -> Io<ops::Bind> {
        Io::from_op(ops::Bind::new(resource.as_resource().clone(), addr))
    }
}

doc_op! {
    short: "Accepts a connection on a listening socket.",
    syscall: "accept(2)",

    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// async fn accept_example() -> std::io::Result<()> {
    ///     # use lio::api::resource::Resource;
    ///     # let fd = Resource::stdin();
    ///     let (client_fd, addr) = lio::api::accept(&fd).await?;
    ///     println!("Accepted connection: {:?}", client_fd);
    ///     Ok(())
    /// }
    /// ```
    pub fn accept(res: &impl AsResource) -> Io<ops::Accept> {
        Io::from_op(ops::Accept::new(res.as_resource().clone()))
    }
}

doc_op! {
    short: "Accepts a connection on a Unix domain socket.",
    syscall: "accept(2)",

    ///
    /// Unlike [`accept`], this function does not attempt to parse the peer address
    /// into a `SocketAddr`, making it suitable for Unix domain sockets.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// async fn accept_unix_example() -> std::io::Result<()> {
    ///     # use lio::api::resource::Resource;
    ///     # let fd = Resource::stdin();
    ///     let client_fd = lio::api::accept_unix(&fd).await?;
    ///     println!("Accepted Unix connection: {:?}", client_fd);
    ///     Ok(())
    /// }
    /// ```
    pub fn accept_unix(res: &impl AsResource) -> Io<ops::AcceptUnix> {
        Io::from_op(ops::AcceptUnix::new(res.as_resource().clone()))
    }
}

/// Creates a streaming accept operation that yields multiple connections.
///
/// Returns an [`IoStreamBuilder`](io::IoStreamBuilder) that yields connections
/// as they arrive on the listening socket. Call `.with_lio()` to bind a Lio
/// instance, then use `.next().await` to accept each connection.
///
/// # Example
///
/// ```rust,no_run
/// use lio::{Lio, api};
/// use lio::api::resource::Resource;
///
/// async fn server(lio: &Lio, listener: &Resource) -> std::io::Result<()> {
///     let mut stream = api::accept_stream(listener).with_lio(lio);
///     while let Some(result) = stream.next().await {
///         let (client, addr) = result?;
///         println!("Accepted connection from {}", addr);
///         // Spawn a task to handle the client...
///     }
///     Ok(())
/// }
/// ```
///
/// # Comparison with [`accept`]
///
/// - `accept()` accepts a single connection and completes
/// - `accept_stream()` continues accepting until the socket closes or errors
///
/// # Platform Notes
///
/// - **io_uring (Linux)**: Will use multishot accept when available, allowing
///   a single syscall submission to accept multiple connections.
/// - **kqueue/epoll**: Resubmits after each accepted connection.
pub fn accept_stream(
  res: &impl AsResource,
) -> io::IoStreamBuilder<ops::AcceptStream> {
  io::IoStreamBuilder::from_op(ops::AcceptStream::new(
    res.as_resource().clone(),
  ))
}

doc_op! {
    short: "Marks a socket as listening for incoming connections.",
    syscall: "listen(2)",

    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// async fn listen_example() -> std::io::Result<()> {
    ///     # use lio::api::resource::Resource;
    ///     # let fd = Resource::stdin();
    ///     lio::api::listen(&fd, 128).await?;
    ///     println!("Socket is now listening for connections");
    ///     Ok(())
    /// }
    /// ```
    pub fn listen(res: &impl AsResource, backlog: i32) -> Io<ops::Listen> {
        Io::from_op(ops::Listen::new(res.as_resource().clone(), backlog))
    }
}

doc_op! {
    short: "Connects a socket to a remote address.",
    syscall: "connect(2)",

    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// use std::net::SocketAddr;
    ///
    /// async fn connect_example() -> std::io::Result<()> {
    ///     # use lio::api::resource::Resource;
    ///     # let fd = Resource::stdin();
    ///     let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
    ///     lio::api::connect(&fd, addr).await?;
    ///     println!("Connected to remote address");
    ///     Ok(())
    /// }
    /// ```
    pub fn connect(res: &impl AsResource, addr: SocketAddr) -> Io<ops::Connect> {
        Io::from_op(ops::Connect::new(res.as_resource().clone(), addr))
    }
}

doc_op! {
    short: "Sends data on a connected socket.",
    syscall: "send(2)",

    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// async fn send_example() -> std::io::Result<()> {
    ///     # use lio::api::resource::Resource;
    ///     # let fd = Resource::stdin();
    ///     let data = b"Hello, server!".to_vec();
    ///     let (bytes_sent, _buf) = lio::api::send(&fd, data, None).await;
    ///     println!("Sent {} bytes", bytes_sent?);
    ///     Ok(())
    /// }
    /// ```
    pub fn send<B>(res: &impl AsResource, buf: B, flags: Option<i32>) -> Io<ops::Send<B>>
    where
        B: IoBuf
    {
        Io::from_op(ops::Send::new(res.as_resource().clone(), buf, flags))
    }
}

doc_op! {
    short: "Receives data over a socket into provided buffer.",
    syscall: "recv(2)",
    doc_link: "https://man7.org/linux/man-pages/man2/recv.2.html",

    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// async fn recv_example() -> std::io::Result<()> {
    ///     # use lio::api::resource::Resource;
    ///     # let fd = Resource::stdin();
    ///     let buffer = vec![0u8; 1024];
    ///     let (res_bytes_received, buf) = lio::api::recv(&fd, buffer, None).await;
    ///     let bytes_received = res_bytes_received? as usize;
    ///     println!("Received {} bytes: {:?}", bytes_received, &buf[..bytes_received]);
    ///     Ok(())
    /// }
    /// ```
    pub fn recv<B>(res: &impl AsResource, buf: B, flags: Option<i32>) -> Io<ops::Recv<B>>
    where
        B: IoBufMut
    {
        Io::from_op(ops::Recv::new(res.as_resource().clone(), buf, flags))
    }
}

doc_op! {
    short: "Sends data to a specific address over an unconnected socket.",
    syscall: "sendto(2)",
    doc_link: "https://man7.org/linux/man-pages/man2/sendto.2.html",

    ///
    /// Unlike [`send`], this function allows sending data to a specific destination
    /// address without first connecting the socket. This is commonly used with UDP sockets.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// use std::net::SocketAddr;
    ///
    /// async fn sendto_example() -> std::io::Result<()> {
    ///     # use lio::api::resource::Resource;
    ///     # let fd = Resource::stdin();
    ///     let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
    ///     let data = b"Hello, server!".to_vec();
    ///     let (bytes_sent, _buf) = lio::api::sendto(&fd, data, addr, None).await;
    ///     println!("Sent {} bytes", bytes_sent?);
    ///     Ok(())
    /// }
    /// ```
    pub fn sendto<B>(res: &impl AsResource, buf: B, addr: SocketAddr, flags: Option<i32>) -> Io<ops::SendTo<B>>
    where
        B: IoBuf
    {
        Io::from_op(ops::SendTo::new(res.as_resource().clone(), buf, addr, flags))
    }
}

doc_op! {
    short: "Receives data and the sender's address from a socket.",
    syscall: "recvfrom(2)",
    doc_link: "https://man7.org/linux/man-pages/man2/recvfrom.2.html",

    ///
    /// Unlike [`recv`], this function also returns the source address of the received
    /// data. This is commonly used with UDP sockets to know where the data came from.
    ///
    /// # Returns
    ///
    /// A tuple containing:
    /// - `io::Result<i32>`: The number of bytes received, or an error
    /// - The buffer (returned for reuse)
    /// - `Option<SocketAddr>`: The source address, if available
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// async fn recvfrom_example() -> std::io::Result<()> {
    ///     # use lio::api::resource::Resource;
    ///     # let fd = Resource::stdin();
    ///     let buffer = vec![0u8; 1024];
    ///     let (res_bytes, buf, peer_addr) = lio::api::recvfrom(&fd, buffer, None).await;
    ///     let bytes_received = res_bytes? as usize;
    ///     if let Some(addr) = peer_addr {
    ///         println!("Received {} bytes from {}", bytes_received, addr);
    ///     }
    ///     Ok(())
    /// }
    /// ```
    pub fn recvfrom<B>(res: &impl AsResource, buf: B, flags: Option<i32>) -> Io<ops::RecvFrom<B>>
    where
        B: IoBufMut
    {
        Io::from_op(ops::RecvFrom::new(res.as_resource().clone(), buf, flags))
    }
}

doc_op! {
    short: "Opens a file relative to a directory file descriptor.",
    syscall: "openat(2)",

    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// use std::ffi::CString;
    ///
    /// async fn openat_example() -> std::io::Result<()> {
    ///     # use lio::api::resource::Resource;
    ///     # use std::os::fd::FromRawFd;
    ///     # let dir = unsafe { Resource::from_raw_fd(libc::AT_FDCWD) };
    ///     let path = CString::new("/tmp/test.txt").unwrap();
    ///     let fd = lio::api::openat(&dir, path, libc::O_RDONLY).await?;
    ///     println!("Opened file: {:?}", fd);
    ///     Ok(())
    /// }
    /// ```
    pub fn openat(dir_res: &impl AsResource, path: CString, flags: i32) -> Io<ops::OpenAt> {
        Io::from_op(ops::OpenAt::new(dir_res.as_resource().clone(), path, flags))
    }
}

doc_op! {
    short: "Copies data between file descriptors without copying to userspace (Linux only).",
    syscall: "tee(2)",

    ///
    /// This operation is only available on Linux systems with io_uring support.
    ///
    /// # Examples
    ///
    /// ```rust
    /// # #[cfg(target_os = "linux")]
    /// # async fn example() -> std::io::Result<()> {
    /// use lio::api;
    /// use lio::api::resource::Resource;
    ///
    /// let fd_in = Resource::stdin();
    /// let fd_out = Resource::stdout();
    /// let bytes_copied = api::tee(&fd_in, fd_out, 1024).await?;
    /// println!("Copied {} bytes", bytes_copied);
    /// # Ok(())
    /// # }
    /// # #[cfg(not(target_os = "linux"))]
    /// # fn example() {}
    /// # fn main() {}
    /// ```
    #[cfg(linux)]
    #[cfg_attr(docsrs, doc(cfg(linux)))]
    pub fn tee(res_in: &impl AsResource, res_out: impl AsResource, size: u32) -> Io<ops::Tee> {
        Io::from_op(ops::Tee::new(res_in.as_resource().clone(), res_out.as_resource().clone(), size))
    }
}

doc_op! {
    short: "Splices data between file descriptors via a pipe (Linux only).",
    syscall: "splice(2)",
    doc_link: "https://man7.org/linux/man-pages/man2/splice.2.html",

    ///
    /// At least one of `fd_in` or `fd_out` must be a pipe. This enables zero-copy
    /// data transfer between a pipe and another file descriptor (socket, file, etc.).
    ///
    /// # Parameters
    ///
    /// - `fd_in`: Source file descriptor
    /// - `off_in`: Offset for source (None for pipes, Some(offset) for files)
    /// - `fd_out`: Destination file descriptor
    /// - `off_out`: Offset for destination (None for pipes, Some(offset) for files)
    /// - `len`: Maximum number of bytes to transfer
    /// - `flags`: Splice flags (e.g., `SPLICE_F_MOVE`, `SPLICE_F_NONBLOCK`)
    ///
    /// # Returns
    ///
    /// Number of bytes transferred on success.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// # #[cfg(target_os = "linux")]
    /// async fn splice_example() -> std::io::Result<()> {
    ///     use lio::api;
    ///     use lio::api::resource::Resource;
    ///
    ///     // Assuming pipe_read and socket are valid Resources
    ///     # let pipe_read = Resource::stdin();
    ///     # let socket = Resource::stdout();
    ///     let bytes = api::splice(&pipe_read, None, &socket, None, 4096, 0).await?;
    ///     println!("Spliced {} bytes", bytes);
    ///     Ok(())
    /// }
    /// ```
    #[cfg(target_os = "linux")]
    #[cfg_attr(docsrs, doc(cfg(target_os = "linux")))]
    pub fn splice(
        fd_in: &impl AsResource,
        off_in: Option<i64>,
        fd_out: &impl AsResource,
        off_out: Option<i64>,
        len: u32,
        flags: u32,
    ) -> Io<ops::Splice> {
        Io::from_op(ops::Splice::new(
            fd_in.as_resource().clone(),
            off_in,
            fd_out.as_resource().clone(),
            off_out,
            len,
            flags,
        ))
    }
}

doc_op! {
    short: "Sends file data to a socket without copying through userspace.",
    syscall: "sendfile(2)",
    doc_link: "https://man7.org/linux/man-pages/man2/sendfile.2.html",

    ///
    /// This is commonly used for serving static files over network sockets,
    /// as it avoids copying data between kernel and user space.
    ///
    /// # Platform Differences
    ///
    /// - **Linux**: `out_fd` can be any file descriptor (socket, file, pipe, etc.)
    /// - **macOS/BSD**: `out_fd` **must** be a socket. Attempting to use a regular
    ///   file will fail with `ENOTSOCK` ("Socket operation on non-socket").
    ///
    /// For copying between files on macOS, use `copy_file_range` on Linux or
    /// fall back to regular read/write operations.
    ///
    /// # Parameters
    ///
    /// - `out_fd`: Destination (socket on macOS/BSD, any fd on Linux)
    /// - `in_fd`: Source file
    /// - `offset`: Starting offset in the source file (None to use current position)
    /// - `count`: Number of bytes to send
    ///
    /// # Returns
    ///
    /// Number of bytes sent on success.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// async fn sendfile_example() -> std::io::Result<()> {
    ///     use lio::api;
    ///     use lio::api::resource::Resource;
    ///
    ///     // Assuming socket and file are valid Resources
    ///     # let socket = Resource::stdout();
    ///     # let file = Resource::stdin();
    ///     let bytes = api::sendfile(&socket, &file, Some(0), 4096).await?;
    ///     println!("Sent {} bytes", bytes);
    ///     Ok(())
    /// }
    /// ```
    #[cfg(unix)]
    #[cfg_attr(docsrs, doc(cfg(unix)))]
    pub fn sendfile(
        out_fd: &impl AsResource,
        in_fd: &impl AsResource,
        offset: Option<i64>,
        count: usize,
    ) -> Io<ops::SendFile> {
        Io::from_op(ops::SendFile::new(
            out_fd.as_resource().clone(),
            in_fd.as_resource().clone(),
            offset,
            count,
        ))
    }
}

doc_op! {
    short: "Copies data between files without going through userspace (Linux only).",
    syscall: "copy_file_range(2)",
    doc_link: "https://man7.org/linux/man-pages/man2/copy_file_range.2.html",

    ///
    /// This performs a server-side copy when possible (e.g., on NFS, Btrfs with reflinks),
    /// avoiding data transfer through the application. Falls back to kernel-space copy
    /// on filesystems that don't support server-side copy.
    ///
    /// # Parameters
    ///
    /// - `fd_in`: Source file
    /// - `off_in`: Starting offset in source file
    /// - `fd_out`: Destination file
    /// - `off_out`: Starting offset in destination file
    /// - `len`: Number of bytes to copy
    /// - `flags`: Reserved, must be 0
    ///
    /// # Returns
    ///
    /// Number of bytes copied on success.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// # #[cfg(target_os = "linux")]
    /// async fn copy_example() -> std::io::Result<()> {
    ///     use lio::api;
    ///     use lio::api::resource::Resource;
    ///
    ///     // Assuming src and dst are valid file Resources
    ///     # let src = Resource::stdin();
    ///     # let dst = Resource::stdout();
    ///     let bytes = api::copy_file_range(&src, 0, &dst, 0, 4096, 0).await?;
    ///     println!("Copied {} bytes", bytes);
    ///     Ok(())
    /// }
    /// ```
    #[cfg(target_os = "linux")]
    #[cfg_attr(docsrs, doc(cfg(target_os = "linux")))]
    pub fn copy_file_range(
        fd_in: &impl AsResource,
        off_in: i64,
        fd_out: &impl AsResource,
        off_out: i64,
        len: usize,
        flags: u32,
    ) -> Io<ops::CopyFileRange> {
        Io::from_op(ops::CopyFileRange::new(
            fd_in.as_resource().clone(),
            off_in,
            fd_out.as_resource().clone(),
            off_out,
            len,
            flags,
        ))
    }
}

doc_op! {
    short: "Reads into multiple buffers with a single syscall (scatter read).",
    syscall: "readv(2)",
    doc_link: "https://man7.org/linux/man-pages/man2/readv.2.html",

    ///
    /// This performs vectored I/O, reading into multiple non-contiguous buffers
    /// in a single syscall. This can be more efficient than multiple `read` calls.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// async fn readv_example() -> std::io::Result<()> {
    ///     # use lio::api::resource::Resource;
    ///     # let fd = Resource::stdin();
    ///     let buf1 = vec![0u8; 512];
    ///     let buf2 = vec![0u8; 512];
    ///     let (result, (buf1, buf2)) = lio::api::readv(&fd, (buf1, buf2)).await;
    ///     let bytes_read = result?;
    ///     println!("Read {} bytes total", bytes_read);
    ///     Ok(())
    /// }
    /// ```
    pub fn readv<B>(res: &impl AsResource, bufs: B) -> Io<ops::ReadV<B>>
    where
        B: IoBufMutVec
    {
        Io::from_op(ops::ReadV::new(res.as_resource().clone(), bufs))
    }
}

doc_op! {
    short: "Writes from multiple buffers with a single syscall (gather write).",
    syscall: "writev(2)",
    doc_link: "https://man7.org/linux/man-pages/man2/writev.2.html",

    ///
    /// This performs vectored I/O, writing from multiple non-contiguous buffers
    /// in a single syscall. This can be more efficient than multiple `write` calls.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// async fn writev_example() -> std::io::Result<()> {
    ///     # use lio::api::resource::Resource;
    ///     # let fd = Resource::stdout();
    ///     let header = b"HTTP/1.1 200 OK\r\n\r\n".to_vec();
    ///     let body = b"Hello, World!".to_vec();
    ///     let (result, (header, body)) = lio::api::writev(&fd, (header, body)).await;
    ///     let bytes_written = result?;
    ///     println!("Wrote {} bytes total", bytes_written);
    ///     Ok(())
    /// }
    /// ```
    pub fn writev<B>(res: &impl AsResource, bufs: B) -> Io<ops::WriteV<B>>
    where
        B: IoBufVec
    {
        Io::from_op(ops::WriteV::new(res.as_resource().clone(), bufs))
    }
}

doc_op! {
    short: "Reads into multiple buffers at a specific offset (scatter read at offset).",
    syscall: "preadv(2)",
    doc_link: "https://man7.org/linux/man-pages/man2/preadv.2.html",

    ///
    /// This performs positional vectored I/O, reading from the given offset
    /// into multiple non-contiguous buffers in a single syscall.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// async fn readv_at_example() -> std::io::Result<()> {
    ///     # use lio::api::resource::Resource;
    ///     # let fd = Resource::stdin();
    ///     let buf1 = vec![0u8; 512];
    ///     let buf2 = vec![0u8; 512];
    ///     let (result, (buf1, buf2)) = lio::api::readv_at(&fd, (buf1, buf2), 1024).await;
    ///     let bytes_read = result?;
    ///     println!("Read {} bytes at offset 1024", bytes_read);
    ///     Ok(())
    /// }
    /// ```
    pub fn readv_at<B>(res: &impl AsResource, bufs: B, offset: i64) -> Io<ops::ReadVAt<B>>
    where
        B: IoBufMutVec
    {
        Io::from_op(ops::ReadVAt::new(res.as_resource().clone(), bufs, offset))
    }
}

doc_op! {
    short: "Writes from multiple buffers at a specific offset (gather write at offset).",
    syscall: "pwritev(2)",
    doc_link: "https://man7.org/linux/man-pages/man2/pwritev.2.html",

    ///
    /// This performs positional vectored I/O, writing from multiple non-contiguous
    /// buffers to the given offset in a single syscall.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// async fn writev_at_example() -> std::io::Result<()> {
    ///     # use lio::api::resource::Resource;
    ///     # let fd = Resource::stdout();
    ///     let header = b"Header data".to_vec();
    ///     let body = b"Body data".to_vec();
    ///     let (result, (header, body)) = lio::api::writev_at(&fd, (header, body), 0).await;
    ///     let bytes_written = result?;
    ///     println!("Wrote {} bytes at offset 0", bytes_written);
    ///     Ok(())
    /// }
    /// ```
    pub fn writev_at<B>(res: &impl AsResource, bufs: B, offset: i64) -> Io<ops::WriteVAt<B>>
    where
        B: IoBufVec
    {
        Io::from_op(ops::WriteVAt::new(res.as_resource().clone(), bufs, offset))
    }
}

/// Waits for a child process to change state.
///
/// This is the async equivalent of `waitid(2)`. It waits for a child process
/// to exit, stop, or continue, and returns information about the state change.
///
/// # Parameters
///
/// - `target`: What process(es) to wait for ([`WaitTarget`](ops::WaitTarget))
/// - `options`: What state changes to wait for ([`WaitOptions`](ops::WaitOptions))
///
/// # Returns
///
/// Returns `Ok(Some(WaitStatus))` if a child changed state, `Ok(None)` if
/// `WNOHANG` was specified and no child was ready, or an error.
///
/// # Example
///
/// ```rust,no_run
/// use lio::api;
/// use lio::api::ops::{WaitTarget, WaitOptions};
///
/// async fn wait_for_child(pid: i32) -> std::io::Result<()> {
///     // Wait for a specific child to exit
///     let status = api::waitid(
///         WaitTarget::Pid(pid),
///         WaitOptions::EXITED,
///     ).await?;
///
///     if let Some(status) = status {
///         if status.exited() {
///             println!("Child {} exited with code {:?}", status.pid, status.exit_code());
///         } else if status.signaled() {
///             println!("Child {} killed by signal {:?}", status.pid, status.signal());
///         }
///     }
///     Ok(())
/// }
/// ```
///
/// # Platform Support
///
/// - **Linux (io_uring 6.7+)**: Uses `IORING_OP_WAITID` for async waiting.
/// - **Other platforms**: Uses blocking `waitid()` syscall.
#[cfg(unix)]
#[cfg_attr(docsrs, doc(cfg(unix)))]
pub fn waitid(
  target: ops::WaitTarget,
  options: ops::WaitOptions,
) -> Io<ops::Waitid> {
  Io::from_op(ops::Waitid::new(target, options))
}

/// Spawns a new process.
///
/// Uses `posix_spawn()` to create a new process. Returns the child's PID
/// on success, which can then be waited on using [`waitid`].
///
/// # Parameters
///
/// - `path`: Path to the executable
/// - `argv`: Command-line arguments (`argv[0]` should be the program name)
/// - `envp`: Environment variables as "KEY=value" strings, or None to inherit
///
/// # Example
///
/// ```rust,no_run
/// use lio::api;
/// use lio::api::ops::{WaitTarget, WaitOptions};
/// use std::ffi::CString;
///
/// async fn run_command() -> std::io::Result<()> {
///     let path = CString::new("/bin/echo").unwrap();
///     let argv = vec![
///         CString::new("echo").unwrap(),
///         CString::new("hello").unwrap(),
///     ];
///
///     // Spawn the process (inherits environment)
///     let pid = api::spawn(path, argv, None).await?;
///     println!("Spawned process with PID: {}", pid);
///
///     // Wait for it to exit
///     let status = api::waitid(WaitTarget::Pid(pid), WaitOptions::EXITED).await?;
///     if let Some(s) = status {
///         println!("Exit code: {:?}", s.exit_code());
///     }
///     Ok(())
/// }
/// ```
#[cfg(unix)]
#[cfg_attr(docsrs, doc(cfg(unix)))]
pub fn spawn(
  path: std::ffi::CString,
  argv: Vec<std::ffi::CString>,
  envp: Option<Vec<std::ffi::CString>>,
) -> Io<ops::Spawn> {
  Io::from_op(ops::Spawn::new(path, argv, envp))
}

/// Applies or removes a lock on an open file.
///
/// This provides whole-file locking with shared (read) and exclusive (write) modes.
///
/// # Lock Operations
///
/// Use constants from [`ops::lock`]:
/// - [`lock::LOCK_SH`][ops::lock::LOCK_SH]: Shared lock (multiple readers allowed)
/// - [`lock::LOCK_EX`][ops::lock::LOCK_EX]: Exclusive lock (single writer)
/// - [`lock::LOCK_UN`][ops::lock::LOCK_UN]: Unlock
///
/// Add [`lock::LOCK_NB`][ops::lock::LOCK_NB] (e.g., `LOCK_EX | LOCK_NB`) for non-blocking behavior.
/// Without `LOCK_NB`, the call blocks until the lock can be acquired.
///
/// # Platform-specific behavior
///
/// This function corresponds to `flock()` on Unix and `LockFileEx`/`UnlockFileEx`
/// on Windows. Note that this may change in the future.
///
/// - **Unix**: Advisory locking. Other processes can ignore locks if they don't
///   cooperate by also using flock.
/// - **Windows**: Mandatory locking enforced by the OS. Locking a file will fail
///   if the file is opened only for append. To lock a file, open it with
///   `.read(true)`, `.read(true).append(true)`, or `.write(true)`.
///
/// # Example
///
/// ```rust,no_run
/// use lio::api;
/// use lio::api::ops::lock;
///
/// async fn lock_example() -> std::io::Result<()> {
///     # use lio::api::resource::Resource;
///     # let file = Resource::stdin();
///     // Acquire exclusive lock (blocks until available)
///     api::flock(&file, lock::LOCK_EX).await?;
///
///     // ... critical section ...
///
///     // Release lock
///     api::flock(&file, lock::LOCK_UN).await?;
///     Ok(())
/// }
/// ```
pub fn flock(fd: &impl AsResource, operation: i32) -> Io<ops::Flock> {
  Io::from_op(ops::Flock::new(fd.as_resource().clone(), operation))
}

/// Reads directory entries from an open directory file descriptor.
///
/// This is a low-level operation that reads raw directory entries into a buffer.
/// The buffer should be large enough to hold at least one entry (typically 4096 bytes
/// or more is recommended).
///
/// # Returns
///
/// A tuple of `(Result<bytes_read>, buffer, parsed_entries)`:
/// - `bytes_read`: Number of bytes read (0 indicates end of directory)
/// - `buffer`: The buffer, returned for reuse
/// - `parsed_entries`: Vector of parsed `DirEntry` structs
///
/// # Example
///
/// ```rust,no_run
/// use lio::api;
/// use std::ffi::CString;
///
/// async fn list_directory() -> std::io::Result<()> {
///     # use lio::api::resource::Resource;
///     # use std::os::fd::FromRawFd;
///     // Open directory
///     let cwd = unsafe { Resource::from_raw_fd(libc::AT_FDCWD) };
///     let dir = api::openat(&cwd, CString::new("/tmp").unwrap(),
///                           libc::O_RDONLY | libc::O_DIRECTORY).await?;
///
///     let mut buf = vec![0u8; 4096];
///     loop {
///         let (result, new_buf, entries) = api::getdents(&dir, buf).await;
///         let bytes_read = result?;
///         buf = new_buf;
///
///         if bytes_read == 0 {
///             break; // End of directory
///         }
///
///         for entry in entries {
///             println!("{:?} (type: {})", entry.name, entry.file_type);
///         }
///     }
///     Ok(())
/// }
/// ```
///
/// # Platform Support
///
/// - Linux: Uses `getdents64` syscall
/// - macOS: Uses `getdirentries`
/// - FreeBSD/DragonFly: Uses `getdirentries`
#[cfg(unix)]
#[cfg_attr(docsrs, doc(cfg(unix)))]
pub fn getdents<B: crate::buf::IoBufMut>(
  fd: &impl AsResource,
  buf: B,
) -> Io<ops::GetDents<B>> {
  Io::from_op(ops::GetDents::new(fd.as_resource().clone(), buf))
}

/// Waits for a signal from the specified signal set.
///
/// This operation blocks until one of the signals in the set is delivered,
/// then returns the signal number. The signal must be blocked (via sigprocmask)
/// before calling this function, otherwise the signal may be delivered to the
/// default handler instead.
///
/// # Example
///
/// ```rust,no_run
/// use lio::api;
/// use lio::api::ops::SignalSet;
///
/// async fn wait_for_signals() -> std::io::Result<()> {
///     // Create signal set for SIGTERM and SIGINT
///     let mut signals = SignalSet::empty();
///     signals.add(libc::SIGTERM);
///     signals.add(libc::SIGINT);
///
///     // Block signals before waiting (important!)
///     // Use libc directly to block the signals
///     unsafe {
///         let mut sigset: libc::sigset_t = std::mem::zeroed();
///         libc::sigemptyset(&mut sigset);
///         libc::sigaddset(&mut sigset, libc::SIGTERM);
///         libc::sigaddset(&mut sigset, libc::SIGINT);
///         libc::sigprocmask(libc::SIG_BLOCK, &sigset, std::ptr::null_mut());
///     }
///
///     // Wait for signal
///     let sig = api::signal(signals).await?;
///     println!("Received signal: {}", sig);
///
///     Ok(())
/// }
/// ```
///
/// # Platform Support
///
/// - Linux: Uses `signalfd`
/// - BSD/macOS: Uses kqueue `EVFILT_SIGNAL`
#[cfg(unix)]
#[cfg_attr(docsrs, doc(cfg(unix)))]
pub fn signal(signals: ops::SignalSet) -> Io<ops::Signal> {
  Io::from_op(ops::Signal::new(signals))
}

/// Connects to a Unix domain socket or other non-IP address.
///
/// This is a low-level function that accepts a pre-built `sockaddr_storage`
/// and length. For Unix domain sockets, prefer using `net::unix::UnixStream::connect`.
#[cfg(unix)]
#[cfg_attr(docsrs, doc(cfg(unix)))]
pub fn connect_unix(
  fd: &impl AsResource,
  addr: libc::sockaddr_storage,
  len: libc::socklen_t,
) -> Io<ops::Connect> {
  Io::from_op(ops::Connect::from_storage(fd.as_resource().clone(), addr, len))
}
