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
pub mod ops;
pub mod resource;
use crate::{api::resource::AsResource, buf::BufLike};
use io::Io;
use std::{ffi::CString, net::SocketAddr, time::Duration};

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
    short: "Issues a timeout that returns after duration seconds.",

    pub fn timeout(duration: Duration) -> Io<ops::Timeout> {
        Io::from_op(ops::Timeout::new(duration))
    }
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
    pub fn write<B>(res: &impl AsResource, buf: B) -> Io<ops::Write<B>>
    where
        B: BufLike + std::marker::Send + Sync
    {
        Io::from_op(ops::Write::new(res.as_resource().clone(), buf))
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
    pub fn write_at<B>(res: &impl AsResource, buf: B, offset: i64) -> Io<ops::WriteAt<B>>
    where
        B: BufLike + std::marker::Send + Sync
    {
        Io::from_op(ops::WriteAt::new(res.as_resource().clone(), buf, offset))
    }
}

doc_op! {
    short: "Reads resource into provided buffer.",
    syscall: "read(2)",
    doc_link: "https://man7.org/linux/man-pages/man2/read.2.html",

    pub fn read<B>(res: &impl AsResource, mem: B) -> Io<ops::Read<B>>
    where
        B: BufLike + std::marker::Send + Sync
    {
        Io::from_op(ops::Read::new(res.as_resource().clone(), mem))
    }
}

doc_op! {
  short: "Reads resource into provided buffer with a offset.",
  syscall: "pread(2)",
  doc_link: "https://man7.org/linux/man-pages/man2/pwrite.2.html",

  pub fn read_at<B>(res: &impl AsResource, mem: B, offset: i64) -> Io<ops::ReadAt<B>>
  where
      B: BufLike + std::marker::Send + Sync
  {
    Io::from_op(ops::ReadAt::new(res.as_resource().clone(), mem, offset))
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
        B: BufLike + std::marker::Send + Sync
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
        B: BufLike + std::marker::Send + Sync
    {
        Io::from_op(ops::Recv::new(res.as_resource().clone(), buf, flags))
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
    /// #[cfg(linux)]
    /// async fn tee_example() -> std::io::Result<()> {
    ///     # let fd_in = 0;
    ///     # let fd_out = 0;
    ///     let bytes_copied = lio::tee(fd_in, fd_out, 1024).await?;
    ///     println!("Copied {} bytes", bytes_copied);
    ///     Ok(())
    /// }
    /// ```
    #[cfg(linux)]
    #[cfg_attr(docsrs, doc(cfg(linux)))]
    pub fn tee(res_in: &impl AsResource, res_out: impl AsResource, size: u32) -> Io<ops::Tee> {
        Io::from_op(ops::Tee::new(res_in.as_resource().clone(), res_out.as_resource().clone(), size))
    }
}
