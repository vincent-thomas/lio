#![allow(private_bounds)]
#![cfg_attr(docsrs, feature(doc_cfg))]

//! # Lio - Low-Level Async I/O Library
//!
//! Lio is a low-level, high-performance async I/O library that provides direct access to
//! platform-native I/O mechanisms. It uses the most efficient I/O available (io_uring/IOCP(soon),
//! kqueue/epoll, regular syscalls).
//!
//! ## Key Characteristics
//!
//! - **Low-level**: Direct syscall wrappers with minimal abstraction
//! - **Manual resource management**: File descriptors and buffers are user-managed
//! - **Zero-copy where possible**: Buffer ownership is transferred to avoid copies
//! - **Platform-native**: Uses the most efficient I/O mechanism per platform
//! - **Flexible APIs**: Choose between async/await, callbacks, or channels
//!
//! ## Platform Support
//!
//! | Platform   | I/O Mechanism | Status      |
//! |------------|---------------|-------------|
//! | Linux      | io_uring      | Supported   |
//! | macOS      | kqueue        | Supported   |
//! | Other Unix | epoll/poll    | Supported   |
//! | Windows    | IOCP          | Planned     |
//!
//! ## Getting Started
//!
//! All I/O operations require initializing the driver first:
//!
//! ```rust
//! # use std::os::fd::RawFd;
//! lio::init();
//!
//! // Perform I/O operations...
//! # let fd: RawFd = 1;
//! # let data = b"Hello, World!\n".to_vec();
//!
//! // Clean up when done
//! lio::exit();
//! ```
//!
//! ## Core Concepts
//!
//! ### The Progress Type
//!
//! All operations return a [`Progress<T>`] which represents an in-flight I/O operation.
//! You can consume it in multiple ways:
//!
//! ```rust
//! use lio::resource::Resource;
//!
//! lio::init();
//!
//! let resource: Resource = 1;
//! let data = b"Hello\n".to_vec();
//!
//! // 1. Async/await (requires async runtime, lio is runtime-independent)
//! async fn async_example(res: Resource, data: Vec<u8>) {
//!     let (result, buf) = lio::write_with_buf(res, data, 0).await;
//!     println!("Wrote {} bytes", result.unwrap());
//! }
//!
//! // 2. Blocking call
//! let (result, buf) = lio::write_with_buf(resource, data.clone(), 0).blocking();
//!
//! // 3. Channel-based
//! let receiver = lio::write_with_buf(resource, data.clone(), 0).send();
//! let (result, buf) = receiver.recv();
//!
//! // 4. Callback-based
//! lio::write_with_buf(resource, data, 0).when_done(|(result, buf)| {
//!     println!("Operation completed: {:?}", result);
//! });
//! lio::exit();
//! ```
//!
//! ## Resource Management
//!
//! This library does **not** automatically close file descriptors or clean up resources.
//! You must manually call [`close`] on file descriptors when done:
//!
//! ```rust
//! use std::ffi::CString;
//! # lio::init();
//!
//! async fn proper_cleanup() -> std::io::Result<()> {
//!     let path = CString::new("/tmp/test").unwrap();
//!     let fd = lio::openat(libc::AT_FDCWD, path, libc::O_RDONLY).await?;
//!
//!     // Use fd...
//!
//!     // Always close when done
//!     lio::close(fd).await?;
//!     Ok(())
//! }
//! # lio::exit();
//! ```
//!
//! ## Buffer Ownership
//!
//! Operations that use buffers (read, write, send, recv) take ownership and return
//! the buffer along with the result. This enables zero-copy I/O:
//!
//! ```rust
//! # use std::os::fd::RawFd;
//! # lio::init();
//! # let fd: RawFd = 1;
//! let buffer = vec![0u8; 1024];
//! let (bytes_read, buffer) = lio::read_with_buf(fd, buffer, 0).blocking();
//! // buffer is returned and can be reused
//! # lio::exit();
//! ```
//!
//! ## Safety Considerations
//!
//! - File descriptors must be valid for the operation's lifetime
//! - Buffers must not be aliased while operations are in-flight
//! - The driver must be initialized before performing any I/O
//! - Call [`exit`] to properly clean up driver resources

pub mod buf;
#[cfg(feature = "unstable_ffi")]
pub mod ffi;
pub mod resource;
mod store;
use std::{ffi::CString, net::SocketAddr, os::fd::RawFd, time::Duration};
mod blocking_queue;

pub use buf::BufResult;

#[macro_use]
mod macros;

pub mod driver;

pub mod op;
use op::*;

#[path = "./registration/registration.rs"]
mod registration;

mod backends;
mod worker;

#[cfg_attr(docsrs, doc(hidden))]
pub mod test_utils;

use crate::{
  backends::IoDriver,
  buf::{BufLike, BufStore},
  driver::{Driver, TryInitError},
  resource::Resource,
};

doc_op! {
    short: "very nice",
    syscall: "shutdown(2)",
    doc_link: "https://man7.org/linux/man-pages/man2/shutdown.2.html",

    ///
    /// Shuts down the read, write, or both halves of this connection.
    /// Read more about how [`Shutdown`] affects this call.
    pub fn shutdown(res: impl Into<Resource>, how: i32) -> Progress<Shutdown> {
        Progress::from_op(Shutdown::new(res.into(), how))
    }
}

doc_op! {
    short: "Issues a timeout that returns after duration seconds.",

    pub fn timeout(duration: Duration) -> Progress<Timeout> {
        Progress::from_op(Timeout::new(duration))
    }
}

doc_op!(
  short: "Create a soft-link.",
  syscall: "symlinkat(2)",
  doc_link: "https://man7.org/linux/man-pages/man2/symlink.2.html",

  pub fn symlinkat(dir_res: impl Into<Resource>, target: CString, linkpath: CString) -> Progress<SymlinkAt> {
    Progress::from_op(SymlinkAt::new(dir_res.into(), target, linkpath))
  }
);

doc_op!(
  short: "Create a hard-link",
  syscall: "linkat(2)",
  doc_link: "https://man7.org/linux/man-pages/man2/linkat.2.html",

  pub fn linkat(old_dir_res: impl Into<Resource>, old_path: CString, new_dir_res: impl Into<Resource>, linkpath: CString) -> Progress<LinkAt> {
    Progress::from_op(LinkAt::new(old_dir_res.into(), old_path, new_dir_res.into(), linkpath))
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
    pub fn fsync(res: impl Into<Resource>) -> Progress<Fsync> {
        Progress::from_op(Fsync::new(res.into()))
    }
}

doc_op! {
    short: "Writes data from buffer to file descriptor.",
    syscall: "pwrite(2)",
    doc_link: "https://man7.org/linux/man-pages/man2/pwrite.2.html",

    ///
    /// # Offset Behavior
    ///
    /// - **Seekable fd, offset ≥ 0**: Write at specified offset (does not advance file cursor).
    /// - **Seekable fd, offset -1**: Write at current file position and advance the cursor.
    /// - **Non-seekable fd** (e.g., pipes, sockets, stdout): Offset is ignored; behaves like `write(2)`.
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
    pub fn write_with_buf<B>(res: impl Into<Resource>, buf: B, offset: i64) -> Progress<Write<B>>
    where
        B: BufLike + std::marker::Send + Sync
    {
        Progress::from_op(Write::new(res.into(), buf, offset))
    }
}

doc_op! {
    short: "Reads resource into provided buffer.",
    syscall: "pread(2)",
    doc_link: "https://man7.org/linux/man-pages/man2/pwrite.2.html",

    pub fn read_with_buf<B>(res: impl Into<Resource>, mem: B, offset: i64) -> Progress<Read<B>>
    where
        B: BufLike + std::marker::Send + Sync
    {
        Progress::from_op(Read::new(res.into(), mem, offset))
    }
}

doc_op! {
  short: "Shortcut, uses [`lio::read_with_buf`](crate::read_with_buf) with [`LentBuf`](crate::buf::LentBuf) as mem field.",
  pub fn read<'a>(res: impl Into<Resource>, offset: i64) -> Progress<Read<crate::buf::LentBuf<'a>>> {
    let buf = Driver::get().try_lend_buf().unwrap();
    Progress::from_op(Read::new(res.into(), buf, offset))
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
    pub fn truncate(res: impl Into<Resource>, len: u64) -> Progress<Truncate> {
        Progress::from_op(Truncate::new(res.into(), len))
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
    pub fn socket(domain: libc::c_int, ty: libc::c_int, proto: libc::c_int) -> Progress<Socket> {
        Progress::from_op(Socket::new(domain, ty, proto))
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
    pub fn bind(resource: impl Into<Resource>, addr: SocketAddr) -> Progress<Bind> {
        Progress::from_op(Bind::new(resource.into(), addr))
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
    pub fn accept(fd: impl Into<Resource>) -> Progress<Accept> {
        Progress::from_op(Accept::new(fd.into()))
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
    pub fn listen(res: impl Into<Resource>, backlog: i32) -> Progress<Listen> {
        Progress::from_op(Listen::new(res.into(), backlog))
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
    pub fn connect(res: impl Into<Resource>, addr: SocketAddr) -> Progress<Connect> {
        Progress::from_op(Connect::new(res.into(), addr))
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
    pub fn send_with_buf<B>(res: impl Into<Resource>, buf: B, flags: Option<i32>) -> Progress<Send<B>>
    where
        B: BufLike + std::marker::Send + Sync
    {
        Progress::from_op(Send::new(res.into(), buf, flags))
    }
}

doc_op! {
    short: "Shortcut, uses [`lio::recv_with_buf`](crate::recv_with_buf) with [`crate::buf::LentBuf`] as mem field.",

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
    pub fn recv<'a>(res: impl Into<Resource>, flags: Option<i32>) -> Progress<Recv<crate::buf::LentBuf<'a>>>
    {
        Progress::from_op(Recv::new(res.into(), Driver::get().try_lend_buf().unwrap(), flags))
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
    pub fn recv_with_buf<B>(res: impl Into<Resource>, buf: B, flags: Option<i32>) -> Progress<Recv<B>>
    where
        B: BufLike + std::marker::Send + Sync
    {
        Progress::from_op(Recv::new(res.into(), buf, flags))
    }
}

doc_op! {
    short: "Closes a file descriptor.",
    syscall: "close(2)",

    ///
    /// # Examples
    ///
    /// ```rust
    /// async fn close_example() -> std::io::Result<()> {
    ///     # let fd = 0;
    ///     lio::close(fd).await?;
    ///     println!("File descriptor closed successfully");
    ///     Ok(())
    /// }
    /// ```
    pub fn close(res: impl Into<Resource>) -> Progress<Close> {
        Progress::from_op(Close::new(res.into()))
    }
}

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
    pub fn openat(dir_res: impl Into<Resource>, path: CString, flags: i32) -> Progress<OpenAt> {
        Progress::from_op(OpenAt::new(dir_res.into(), path, flags))
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
    pub fn tee(res_in: impl Into<Resource>, res_out: impl Into<Resource>, size: u32) -> Progress<Tee> {
        Progress::from_op(Tee::new(res_in.into(), res_out.into(), size))
    }
}

// / Shut down the lio I/O driver background thread(s) and release OS resources.
// /
// / After calling this, further I/O operations in this process are unsupported.
// / Calling shutdown more than once will panic.
// pub fn exit() {
//   Driver::shutdown()
// }

pub fn tick() {
  Driver::get().tick(false)
}

/// Deallocates the lio I/O driver, freeing all resources.
///
/// This must be called after `exit()` to properly clean up the driver.
/// Calling this before `exit()` or when the driver is not initialized will panic.
pub fn exit() {
  Driver::get().exit()
}

pub fn init() {
  crate::try_init().expect("lio is already initialised.");
}

#[cfg(linux)]
type Default = backends::IoUring;
#[cfg(not(linux))]
type Default = backends::pollingv2::Poller;

pub fn try_init() -> Result<(), TryInitError> {
  crate::try_init_with_driver::<Default>()
}

pub fn try_init_with_driver<B: IoDriver>() -> Result<(), TryInitError> {
  crate::try_init_with_driver_and_capacity::<B>(1024)
}

pub fn try_init_with_driver_and_capacity<D>(
  cap: usize,
) -> Result<(), TryInitError>
where
  D: IoDriver,
{
  Driver::try_init_with_capacity_and_bufstore::<D>(cap, BufStore::default())
}

pub fn try_init_with_driver_and_capacity_and_bufstore<D>(
  cap: usize,
  buf_store: BufStore,
) -> Result<(), TryInitError>
where
  D: IoDriver,
{
  Driver::try_init_with_capacity_and_bufstore::<D>(cap, buf_store)
}
