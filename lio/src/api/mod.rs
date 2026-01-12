//! lio's low-level I/O API.
//!
//! This module contains all the syscall-oriented I/O operations provided by lio.
//! Each function maps directly to a system call and returns a [`Progress`] handle
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
//! Lio is compatible with any synchronisation method, through the [`Progress`] type:
//! - [async](progress::Progress#impl-IntoFuture-for-Progress<'a,+T>) (runtime-independent).
//! - [Callbacks](progress::Progress::when_done).
//! - Channels: [get receiver](progress::Progress::send) *or* [send with your own](progress::Progress::send_with).
//!
//! # Buffer Ownership
//!
//! Operations that use buffers ([`read`], [`write`](write()), [`recv`], [`send`]) take
//! ownership of the buffer and return it along with the result. This enables
//! zero-copy I/O while ensuring memory safety:
//!
//! ```ignore
//! let buf = vec![0u8; 1024];
//! let (result, buf) = lio::read(fd, buf).await;
//! // buf is returned and can be reused
//! ```
//!
//! # Example: Echo Server
//!
//! ```ignore
//! use lio::api::resource::Resource;
//!
//! async fn echo_server() -> std::io::Result<()> {
//!     // Create and bind socket
//!     let sock = lio::socket(libc::AF_INET, libc::SOCK_STREAM, 0).await?;
//!     let addr = "127.0.0.1:8080".parse().unwrap();
//!     lio::bind(&sock, addr).await?;
//!     lio::listen(&sock, 128).await?;
//!
//!     loop {
//!         // Accept connection
//!         let (client, _addr) = lio::accept(&sock).await?;
//!
//!         // Echo loop
//!         let mut buf = vec![0u8; 1024];
//!         loop {
//!             let (n, buf_back) = lio::recv(&client, buf, None).await;
//!             let n = n?;
//!             if n == 0 { break; }
//!
//!             let (_, buf_back) = lio::send(&client, buf_back, None).await;
//!             buf = buf_back.0?;
//!         }
//!     }
//! }
//! ```
//!
//! # See Also
//!
//! - [`Progress`] - Operation handle with multiple completion modes
//! - [`Resource`](crate::api::resource::Resource) - Reference-counted file descriptor wrapper
//! - [`crate::buf`] - Buffer types and pooling

pub mod ops;
pub mod progress;
pub mod resource;
use crate::{api::resource::AsResource, buf::BufLike};
use progress::Progress;
use std::{ffi::CString, net::SocketAddr, time::Duration};

doc_op! {
    short: "does nothing. maybe useful for testing?",

    pub fn nop() -> Progress<'static, ops::Nop> {
        Progress::from_op(ops::Nop)
    }
}

doc_op! {
    short: "very nice",
    syscall: "shutdown(2)",
    doc_link: "https://man7.org/linux/man-pages/man2/shutdown.2.html",

    ///
    /// Shuts down the read, write, or both halves of this connection.
    pub fn shutdown(res: impl AsResource, how: i32) -> Progress<'static, ops::Shutdown> {
        Progress::from_op(ops::Shutdown::new(res.as_resource().clone(), how))
    }
}

doc_op! {
    short: "Issues a timeout that returns after duration seconds.",

    pub fn timeout(duration: Duration) -> Progress<'static, ops::Timeout> {
        Progress::from_op(ops::Timeout::new(duration))
    }
}

doc_op!(
  short: "Create a soft-link.",
  syscall: "symlinkat(2)",
  doc_link: "https://man7.org/linux/man-pages/man2/symlink.2.html",

  pub fn symlinkat(dir_res: impl AsResource, target: CString, linkpath: CString) -> Progress<'static, ops::SymlinkAt> {
    Progress::from_op(ops::SymlinkAt::new(dir_res.as_resource().clone(), target, linkpath))
  }
);

doc_op!(
  short: "Create a hard-link",
  syscall: "linkat(2)",
  doc_link: "https://man7.org/linux/man-pages/man2/linkat.2.html",

  pub fn linkat(old_dir_res: impl AsResource, old_path: CString, new_dir_res: impl AsResource, linkpath: CString) -> Progress<'static, ops::LinkAt> {
    Progress::from_op(ops::LinkAt::new(old_dir_res.as_resource().clone(), old_path, new_dir_res.as_resource().clone(), linkpath))
  }
);

doc_op! {
    short: "Synchronizes file data to storage.",
    syscall: "fsync(2)",

    ///
    /// # Examples
    ///
    /// ```rust
    /// async fn write_example() -> std::io::Result<()> {
    ///     # let fd = 0;
    ///     lio::fsync(fd).await?;
    ///     Ok(())
    /// }
    /// ```
    pub fn fsync(res: impl AsResource) -> Progress<'static, ops::Fsync> {
        Progress::from_op(ops::Fsync::new(res.as_resource().clone()))
    }
}

doc_op! {
    short: "Writes data from buffer to file descriptor.",
    syscall: "write(2)",
    doc_link: "https://man7.org/linux/man-pages/man2/write.2.html",

    ///
    /// # Examples
    ///
    /// ```rust
    /// async fn write_example() -> std::io::Result<()> {
    ///     # let fd = 0;
    ///     let data = b"Hello, World!".to_vec();
    ///     let (result_bytes_written, _buf) = lio::write_with_buf(fd, data, 0).await;
    ///     println!("Wrote {} bytes", result_bytes_written?);
    ///     Ok(())
    /// }
    /// ```
    pub fn write<B>(res: impl AsResource, buf: B) -> Progress<'static, ops::Write<B>>
    where
        B: BufLike + std::marker::Send + Sync
    {
        Progress::from_op(ops::Write::new(res.as_resource().clone(), buf))
    }
}

doc_op! {
    short: "Writes data from buffer to file descriptor. file descriptor cannot be non-seeakble.",
    syscall: "pwrite(2)",
    doc_link: "https://man7.org/linux/man-pages/man2/pwrite.2.html",

    ///
    /// # Errors
    ///
    /// - `EINVAL`: Invalid offset on non-seekable fd.
    ///
    /// # Examples
    ///
    /// ```rust
    /// async fn write_example() -> std::io::Result<()> {
    ///     # let fd = 0;
    ///     let data = b"Hello, World!".to_vec();
    ///     let (result_bytes_written, _buf) = lio::write_with_buf(fd, data, 0).await;
    ///     println!("Wrote {} bytes", result_bytes_written?);
    ///     Ok(())
    /// }
    /// ```
    pub fn write_at<B>(res: impl AsResource, buf: B, offset: i64) -> Progress<'static, ops::WriteAt<B>>
    where
        B: BufLike + std::marker::Send + Sync
    {
        Progress::from_op(ops::WriteAt::new(res.as_resource().clone(), buf, offset))
    }
}

doc_op! {
    short: "Reads resource into provided buffer.",
    syscall: "read(2)",
    doc_link: "https://man7.org/linux/man-pages/man2/read.2.html",

    pub fn read<B>(res: impl AsResource, mem: B) -> Progress<'static, ops::Read<B>>
    where
        B: BufLike + std::marker::Send + Sync
    {
        Progress::from_op(ops::Read::new(res.as_resource().clone(), mem))
    }
}

doc_op! {
  short: "Reads resource into provided buffer with a offset.",
  syscall: "pread(2)",
  doc_link: "https://man7.org/linux/man-pages/man2/pwrite.2.html",

  pub fn read_at<B>(res: impl AsResource, mem: B, offset: i64) -> Progress<'static, ops::ReadAt<B>>
  where
      B: BufLike + std::marker::Send + Sync
  {
    Progress::from_op(ops::ReadAt::new(res.as_resource().clone(), mem, offset))
  }
}

doc_op! {
    short: "Truncates a file to a specified length.",
    syscall: "ftruncate(2)",

    ///
    /// # Examples
    ///
    /// ```rust
    /// async fn truncate_example() -> std::io::Result<()> {
    ///     # let fd = 0;
    ///     lio::truncate(fd, 1024).await?; // Truncate to 1KB
    ///     Ok(())
    /// }
    /// ```
    pub fn truncate(res: impl AsResource, len: u64) -> Progress<'static, ops::Truncate> {
        Progress::from_op(ops::Truncate::new(res.as_resource().clone(), len))
    }
}

doc_op! {
    short: "Creates a new socket with the specified domain, type, and protocol.",
    syscall: "socket(2)",

    ///
    /// # Examples
    ///
    /// ```rust
    /// async fn socket_example() -> std::io::Result<()> {
    ///     // AF_INET (IPv4), SOCK_STREAM (TCP), protocol 0 (default)
    ///     let sock = lio::socket(libc::AF_INET, libc::SOCK_STREAM, 0).await?;
    ///     println!("Created socket with fd: {}", sock);
    ///     Ok(())
    /// }
    /// ```
    pub fn socket(domain: libc::c_int, ty: libc::c_int, proto: libc::c_int) -> Progress<'static, ops::Socket> {
        Progress::from_op(ops::Socket::new(domain, ty, proto))
    }
}

doc_op! {
    short: "Binds a socket to a specific address.",
    syscall: "bind(2)",

    ///
    /// # Examples
    ///
    /// ```rust
    /// use socket2::SockAddr;
    ///
    /// async fn bind_example() -> std::io::Result<()> {
    ///     # let fd = 0;
    ///     let addr = "127.0.0.1:8080".parse::<std::net::SocketAddr>().unwrap();
    ///     lio::bind(fd, addr).await?;
    ///     Ok(())
    /// }
    /// ```
    pub fn bind(resource: impl AsResource, addr: SocketAddr) -> Progress<'static, ops::Bind> {
        Progress::from_op(ops::Bind::new(resource.as_resource().clone(), addr))
    }
}

doc_op! {
    short: "Accepts a connection on a listening socket.",
    syscall: "accept(2)",

    ///
    /// # Examples
    ///
    /// ```rust
    /// use std::mem::MaybeUninit;
    ///
    /// async fn accept_example() -> std::io::Result<()> {
    ///     # let fd = 0;
    ///
    ///     let (client_fd, addr) = lio::accept(fd).await?;
    ///     println!("Accepted connection on fd: {}", client_fd);
    ///     Ok(())
    /// }
    /// ```
    pub fn accept(res: impl AsResource) -> Progress<'static, ops::Accept> {
        Progress::from_op(ops::Accept::new(res.as_resource().clone()))
    }
}

doc_op! {
    short: "Marks a socket as listening for incoming connections.",
    syscall: "listen(2)",

    ///
    /// # Examples
    ///
    /// ```rust
    /// use std::os::fd::RawFd;
    ///
    /// async fn listen_example() -> std::io::Result<()> {
    ///     # let fd = 0;
    ///     lio::listen(fd, 128).await?;
    ///     println!("Socket is now listening for connections");
    ///     Ok(())
    /// }
    /// ```
    pub fn listen(res: impl AsResource, backlog: i32) -> Progress<'static, ops::Listen> {
        Progress::from_op(ops::Listen::new(res.as_resource().clone(), backlog))
    }
}

doc_op! {
    short: "Connects a socket to a remote address.",
    syscall: "connect(2)",

    ///
    /// # Examples
    ///
    /// ```rust
    /// use std::net::SocketAddr;
    ///
    /// async fn connect_example() -> std::io::Result<()> {
    ///     # let fd = 0;
    ///     let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
    ///     lio::connect(fd, addr).await?;
    ///     println!("Connected to remote address");
    ///     Ok(())
    /// }
    /// ```
    pub fn connect(res: impl AsResource, addr: SocketAddr) -> Progress<'static, ops::Connect> {
        Progress::from_op(ops::Connect::new(res.as_resource().clone(), addr))
    }
}

doc_op! {
    short: "Sends data on a connected socket.",
    syscall: "send(2)",

    ///
    /// # Examples
    ///
    /// ```rust
    /// async fn send_example() -> std::io::Result<()> {
    ///     # let fd = 0;
    ///     let data = b"Hello, server!".to_vec();
    ///     let (bytes_sent, _buf) = lio::send_with_buf(fd, data, None).await;
    ///     println!("Sent {} bytes", bytes_sent?);
    ///     Ok(())
    /// }
    /// ```
    pub fn send<B>(res: impl AsResource, buf: B, flags: Option<i32>) -> Progress<'static, ops::Send<B>>
    where
        B: BufLike + std::marker::Send + Sync
    {
        Progress::from_op(ops::Send::new(res.as_resource().clone(), buf, flags))
    }
}

doc_op! {
    short: "Receives data over a socket into provided buffer.",
    syscall: "recv(2)",
    doc_link: "https://man7.org/linux/man-pages/man2/recv.2.html",

    ///
    /// # Examples
    ///
    /// ```rust
    /// async fn recv_example() -> std::io::Result<()> {
    ///     # let fd = 0;
    ///     let mut buffer = vec![0u8; 1024];
    ///     let (res_bytes_received, buf) = lio::recv_with_buf(fd, buffer, None).await;
    ///     let bytes_received = res_bytes_received?;
    ///     println!("Received {} bytes: {:?}", bytes_received, &buf[..bytes_received as usize]);
    ///     Ok(())
    /// }
    /// ```
    pub fn recv<B>(res: impl AsResource, buf: B, flags: Option<i32>) -> Progress<'static, ops::Recv<B>>
    where
        B: BufLike + std::marker::Send + Sync
    {
        Progress::from_op(ops::Recv::new(res.as_resource().clone(), buf, flags))
    }
}

// doc_op! {
//     short: "Closes a file descriptor.",
//     syscall: "close(2)",
//
//     ///
//     /// # Examples
//     ///
//     /// ```rust
//     /// async fn close_example() -> std::io::Result<()> {
//     ///     # let fd = 0;
//     ///     lio::close(fd).await?;
//     ///     println!("File descriptor closed successfully");
//     ///     Ok(())
//     /// }
//     /// ```
//     pub fn close(res: UniqueResource) -> Progress<'static, ops::Close> {
//         Progress::from_op(ops::Close::new(res))
//     }
// }

doc_op! {
    short: "Opens a file relative to a directory file descriptor.",
    syscall: "openat(2)",

    ///
    /// # Examples
    ///
    /// ```rust
    /// use std::ffi::CString;
    ///
    /// async fn openat_example() -> std::io::Result<()> {
    ///     let path = CString::new("/tmp/test.txt").unwrap();
    ///     let fd = lio::openat(libc::AT_FDCWD, path, libc::O_RDONLY).await?;
    ///     println!("Opened file with fd: {}", fd);
    ///     Ok(())
    /// }
    /// ```
    pub fn openat(dir_res: impl AsResource, path: CString, flags: i32) -> Progress<'static, ops::OpenAt> {
        Progress::from_op(ops::OpenAt::new(dir_res.as_resource().clone(), path, flags))
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
    pub fn tee(res_in: impl AsResource, res_out: impl AsResource, size: u32) -> Progress<'static, ops::Tee> {
        Progress::from_op(ops::Tee::new(res_in.as_resource().clone(), res_out.as_resource().clone(), size))
    }
}
