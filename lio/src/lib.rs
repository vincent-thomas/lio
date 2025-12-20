#![allow(private_bounds)]
#![cfg_attr(docsrs, feature(doc_cfg))]

//! # Lio - Platform-Independent Async I/O Library
//!
//! Lio is a high-performance, platform-independent async I/O library that uses
//! the most efficient IO for each platform.
//!
//! ## Features
//! - **Zero-copy operations** where possible.
//! - **Automatic fallback** to blocking operations when async isn't supported.
//! - **Manual control** with high level async API.
//!
//! *Note:* This is a quite low-level library. This library creates os resources
//! (fd's) which it doesn't cleanup automatically.
//!
//! ## Platform support
//!
//! | Platform   | I/O Mechanism            | Status                  |
//! |------------|--------------------------|-------------------------|
//! | Linux      | io_uring                 | Yes                     |
//! | Windows    | IOCP                     | Not supported (planned) |
//! | macOS      | kqueue                   | Yes                     |
//! | Other Unix | poll/epoll/event ports   | Yes                     |
//!
//!
//! ## Quick Start
//!
//! ```rust
//! # #![cfg(feature = "high")]
//! use std::os::fd::RawFd;
//! use std::io;
//!
//! fn handle_result(result: std::io::Result<i32>, buf: Vec<u8>) {
//!   println!("result: {result:?}, buf: {buf:?}");
//! }
//!
//! async fn example() -> std::io::Result<()> {
//!     let fd: RawFd = 1; // stdout
//!     let data = b"Hello, World!\n".to_vec();
//!
//!     // Async API (on "high" feature flag).
//!     let (result, buf) = lio::write(fd, data.clone(), 0).await;
//!     handle_result(result, buf);
//!
//!     // Channel API (on "high" feature flag).
//!     let receiver: lio::BlockingReceiver<(io::Result<i32>, Vec<u8>)> = lio::write(fd, data.clone(), 0).send();
//!     let (result, buf) = receiver.recv();
//!     handle_result(result, buf);
//!
//!     // Callback API.
//!     lio::write(fd, data.clone(), 0).when_done(|(result, buf)| {
//!       handle_result(result, buf);
//!     });
//!
//!     Ok(())
//! }
//! ```
//!
//! **Note**: Only one of these API's can be used for one operation.
//!
//! ## Safety and Threading
//!
//! - The library handles thread management for background I/O processing
//! - Operations can be safely used across different threads
//!
//! ## Error Handling
//!
//! All operations return [`std::io::Result`] or [`BufResult`] for operations
//! that return buffers. Errors are automatically converted from platform-specific
//! error codes to Rust's standard I/O error types.

pub mod buf;
#[cfg(feature = "unstable_ffi")]
pub mod ffi;
mod sync;
use std::{
  ffi::{CString, NulError},
  net::SocketAddr,
  os::fd::RawFd,
  time::Duration,
};

pub use buf::BufResult;

#[macro_use]
mod macros;

pub mod driver;

pub mod op;
use op::*;

#[cfg(target_os = "linux")]
mod liburing;

mod op_registration;

mod backends;

use crate::{
  buf::{Buf, BufLike},
  driver::{Driver, TryInitError},
};

#[cfg(feature = "buf")]
use crate::buf::LentBuf;
use std::path::Path;

macro_rules! impl_op {
  // Internal helper: Generate function with common documentation
  (@impl_fn
    { $desc:expr, $operation:ident, $name:ident, [$($arg:ident : $arg_ty:ty),*], $ret:ty, $detach_safe:expr }
    $( #[$($doc:tt)*] )*
    { $($body:tt)* }
  ) => {
    #[doc = $desc]
    #[doc = "# Returns"]
    #[doc = concat!("This function returns `OperationProgress<", stringify!($operation), ">`.")]
    #[doc = "This function signature is equivalent to:"]
    #[doc = concat!("```ignore\nasync fn ",stringify!($name), "(", stringify!($($arg_ty),*), ") -> ", stringify!($ret), "\n```")]
    #[doc = "# Behavior"]
    #[doc = "As soon as this function is called, the operation is submitted into the io-driver used by the current platform (for example io-uring). If the user then chooses to drop [`OperationProgress`] before the [`Future`] is ready, the operation will **NOT** tried be cancelled, but instead \"detached\"."]
    #[doc = $detach_safe]
    #[doc = "\n\nSee more [what methods are available to the return type](crate::OperationProgress#impl-OperationProgress<T>)."]
    $( #[$($doc)*] )*
    $($body)*
  };

  // !detach variant with error type
  (
    !detach $desc:tt,
    $(#[$($doc:tt)*])*
    $operation:ident, fn $name:ident ( $($arg:ident: $arg_ty:ty),* ) -> $ret:ty ; $err:ty
  ) => {
    use op::$operation;

    impl_op!(@impl_fn
      { $desc, $operation, $name, [$($arg : $arg_ty),*], $ret, "\n\n## Detach safe\nThis method is not [`detach safe`](crate::DetachSafe), which means that resources _**will**_ leak if not handled carefully." }
      $( #[$($doc)*] )*
      {
        pub fn $name($($arg: $arg_ty),*) -> Result<OperationProgress<$operation>, $err> {
          Ok(Driver::submit($operation::new($($arg),*)?))
        }
      }
    );
  };

  // !detach variant without error type
  (
    !detach $desc:tt,
    $(#[$($doc:tt)*])*
    $operation:ident, fn $name:ident ( $($arg:ident: $arg_ty:ty),* ) -> $ret:ty
  ) => {
    impl_op!(@impl_fn
      { $desc, $operation, $name, [$($arg : $arg_ty),*], $ret, "\n\n## Detach safe\nThis method is not [`detach safe`](crate::DetachSafe), which means that resources _**will**_ leak if not handled carefully." }
      $( #[$($doc)*] )*
      {
        pub fn $name($($arg: $arg_ty),*) -> OperationProgress<$operation> {
          Driver::submit($operation::new($($arg),*))
        }
      }
    );
  };

  // With error type - fallible constructor
  (
    $desc:expr,
    $(#[$($doc:tt)*])*
    $operation:ident, fn $name:ident ( $($arg:ident: $arg_ty:ty),* ) -> $ret:ty ; $err:ty
  ) => {
    use op::$operation;

    impl_op!(@impl_fn
      { $desc, $operation, $name, [$($arg : $arg_ty),*], $ret, "" }
      $( #[$($doc)*] )*
      {
        pub fn $name($($arg: $arg_ty),*) -> Result<OperationProgress<$operation>, $err> {
          Ok(Driver::submit($operation::new($($arg),*)?))
        }
      }
    );
  };

  // Without error type - infallible constructor
  (
    $desc:expr,
    $(#[$($doc:tt)*])*
    $operation:ident, fn $name:ident ( $($arg:ident: $arg_ty:ty),* ) -> $ret:ty
  ) => {
    impl_op!(@impl_fn
      { $desc, $operation, $name, [$($arg : $arg_ty),*], $ret, "" }
      $( #[$($doc)*] )*
      {
        pub fn $name($($arg: $arg_ty),*) -> OperationProgress<$operation> {
          Driver::submit($operation::new($($arg),*))
        }
      }
    );
  };

  // Convenience: no description provided
  (
    $(#[$($doc:tt)*])*
    $operation:ty, fn $name:ident ( $($arg:ident: $arg_ty:ty),* ) -> $ret:ty
  ) => {
    impl_op!("", $(#[$($doc)*])* $operation, fn $name($($arg: $arg_ty),*) -> $ret);
  };
}

impl_op!(
 "Shuts socket down.",
 /// # Examples
 ///
 /// ```rust
 /// async fn write_example() -> std::io::Result<()> {
 ///     let socket = lio::socket(socket2::Domain::IPV4, socket2::Type::STREAM, None).await?;
 ///     let how = 0;
 ///     lio::shutdown(socket, how).await?;
 ///     Ok(())
 /// }
 /// ```
 Shutdown, fn shutdown(fd: RawFd, how: i32) -> io::Result<()>
);

impl_op!(
  "Times out something",
  /// # Examples
  ///
  /// ```rust
  /// use lio::timeout;
  /// use std::{time::Duration, os::fd::RawFd};
  ///
  /// async fn write_example() -> std::io::Result<()> {
  ///     timeout(Duration::from_millis(10)).await?;
  ///     Ok(())
  /// }
  /// ```
  Timeout, fn timeout(duration: Duration) -> io::Result<()>
);

impl_op!(
  "Create a soft-link",
  /// # Examples
  ///
  /// ```rust
  /// async fn write_example() -> std::io::Result<()> {
  ///     # let fd = 0;
  ///     // todo
  // ///     let (bytes_written, _buf) = lio::symlinkat(fd).await?;
  ///     Ok(())
  /// }
  /// ```
  SymlinkAt, fn symlinkat(new_dir_fd: RawFd, target: impl AsRef<Path>, linkpath: impl AsRef<Path>) -> io::Result<()> ; NulError
);

impl_op!(
  "Create a hard-link",
  /// # Examples
  ///
  /// ```rust
  /// async fn write_example() -> std::io::Result<()> {
  ///     # let fd = 0;
  ///       // todo
  // ///     let (bytes_written, _buf) = lio::linkat(fd)?.await?;
  ///     Ok(())
  /// }
  /// ```
  LinkAt, fn linkat(old_dir_fd: RawFd, old_path: impl AsRef<Path>, new_dir_fd: RawFd, new_path: impl AsRef<Path>) -> io::Result<()> ; NulError
);

impl_op!(
  "Sync to fd.",
  /// # Examples
  ///
  /// ```rust
  /// async fn write_example() -> std::io::Result<()> {
  ///     # let fd = 0;
  ///     lio::fsync(fd).await?;
  ///     Ok(())
  /// }
  /// ```
  Fsync, fn fsync(fd: RawFd) -> io::Result<()>
);

#[cfg(feature = "buf")]
impl_op!(@impl_fn {
    "Performs a write operation on a file descriptor. Equivalent to the `pwrite` syscall.

# Offset Behavior

- **Seekable fd, offset ≥ 0**: Write at specified offset (does not advance file cursor).
- **Seekable fd, offset -1**: Write at current file position and advance the cursor.
- **Non-seekable fd** (e.g., pipes, sockets, stdout): Offset is ignored; behaves like `write(2)`.

# Errors

- `EINVAL`: Invalid offset on non-seekable fd.",Write,write,[fd:RawFd,buf:Vec<u8> ,offset:i64],BufResult<i32,Vec<u8>> ,""
}#[doc = r" # Examples"]#[doc = r""]#[doc = r" ```rust"]#[doc = r" async fn write_example() -> std::io::Result<()> {"]#[doc = r"     # let fd = 0;"]#[doc = r#"     let data = b"Hello, World!".to_vec();"#]#[doc = r"     let (result_bytes_written, _buf) = lio::write(fd, data, 0).await;"]#[doc = r#"     println!("Wrote {} bytes", result_bytes_written?);"#]#[doc = r"     Ok(())"]#[doc = r" }"]#[doc = r" ```"]{
    pub fn write(fd: RawFd, offset: i64)-> OperationProgress<Write<LentBuf<'static>>> {
        crate::write_with_buf(fd, Driver::get().try_lend_buf().expect("Couldn't lend buffer"), offset)
    }
});

impl_op!(@impl_fn {
    "Performs a write operation on a file descriptor. Equivalent to the `pwrite` syscall.

# Offset Behavior

- **Seekable fd, offset ≥ 0**: Write at specified offset (does not advance file cursor).
- **Seekable fd, offset -1**: Write at current file position and advance the cursor.
- **Non-seekable fd** (e.g., pipes, sockets, stdout): Offset is ignored; behaves like `write(2)`.

# Errors

- `EINVAL`: Invalid offset on non-seekable fd.",Write,write,[fd:RawFd,buf:Vec<u8> ,offset:i64],BufResult<i32,Vec<u8>> ,""
}#[doc = r" # Examples"]#[doc = r""]#[doc = r" ```rust"]#[doc = r" async fn write_example() -> std::io::Result<()> {"]#[doc = r"     # let fd = 0;"]#[doc = r#"     let data = b"Hello, World!".to_vec();"#]#[doc = r"     let (result_bytes_written, _buf) = lio::write_with_buf(fd, data, 0).await;"]#[doc = r#"     println!("Wrote {} bytes", result_bytes_written?);"#]#[doc = r"     Ok(())"]#[doc = r" }"]#[doc = r" ```"]{
    pub fn write_with_buf<B>(fd: RawFd, buf: B, offset: i64)-> OperationProgress<Write<B>> where B: BufLike {
        Driver::submit(Write::new(fd, Buf::new(buf),offset))
    }
});

#[cfg(feature = "buf")]
impl_op!(@impl_fn {
    "Performs a read operation on a file descriptor. Equivalent of the `pread` syscall.",Read,read,[fd:RawFd,mem:Vec<u8> ,offset:i64],BufResult<i32,Vec<u8>> ,""
}#[doc = r" # Examples"]#[doc = r""]#[doc = r" ```rust"]#[doc = r" async fn read_example() -> std::io::Result<()> {"]#[doc = r"     # let fd = 0;"]#[doc = r"     let mut buffer = vec![0u8; 1024];"]#[doc = r"     let (res_bytes_read, buf) = lio::read_with_buf(fd, buffer, 0).await;"]#[doc = r"     let bytes_read = res_bytes_read?;"]#[doc = r#"     println!("Read {} bytes: {:?}", bytes_read, &buf[..bytes_read as usize]);"#]#[doc = r"     Ok(())"]#[doc = r" }"]#[doc = r" ```"]{
    pub fn read(fd: RawFd, offset: i64)-> OperationProgress<Read<LentBuf<'static>>> {
        crate::read_with_buf(fd, Driver::get().try_lend_buf().expect("Couldn't lend buffer"), offset)
    }
});

impl_op!(@impl_fn {
    "Performs a read operation on a file descriptor. Equivalent of the `pread` syscall.",Read,read,[fd:RawFd,mem:Vec<u8> ,offset:i64],BufResult<i32,Vec<u8>> ,""
}#[doc = r" # Examples"]#[doc = r""]#[doc = r" ```rust"]#[doc = r" async fn read_example() -> std::io::Result<()> {"]#[doc = r"     # let fd = 0;"]#[doc = r"     let mut buffer = vec![0u8; 1024];"]#[doc = r"     let (res_bytes_read, buf) = lio::read_with_buf(fd, buffer, 0).await;"]#[doc = r"     let bytes_read = res_bytes_read?;"]#[doc = r#"     println!("Read {} bytes: {:?}", bytes_read, &buf[..bytes_read as usize]);"#]#[doc = r"     Ok(())"]#[doc = r" }"]#[doc = r" ```"]{
    pub fn read_with_buf<B>(fd: RawFd, mem: B, offset: i64)-> OperationProgress<Read<B>> where B: BufLike {
        Driver::submit(Read::new(fd, Buf::new(mem), offset))
    }
});

impl_op!(
  "Truncates a file to a specified length.",
  /// # Examples
  ///
  /// ```rust
  /// async fn truncate_example() -> std::io::Result<()> {
  ///     # let fd = 0;
  ///     lio::truncate(fd, 1024).await?; // Truncate to 1KB
  ///     Ok(())
  /// }
  /// ```
  Truncate, fn truncate(fd: RawFd, len: u64) -> std::io::Result<()>
);

impl_op!(
  !detach
  "Creates a new socket with the specified domain, type, and protocol.",
  /// # Examples
  ///
  /// ```rust
  /// use socket2::{Domain, Type, Protocol};
  ///
  /// async fn socket_example() -> std::io::Result<()> {
  ///     let sock = lio::socket(Domain::IPV4, Type::STREAM, Some(Protocol::TCP)).await?;
  ///     println!("Created socket with fd: {}", sock);
  ///     Ok(())
  /// }
  /// ```
  Socket, fn socket(domain: socket2::Domain, ty: socket2::Type, proto: Option<socket2::Protocol>) -> std::io::Result<i32>
);

impl_op!(
  "Binds a socket to a specific address.",
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
  ///
  /// ```
  Bind, fn bind(fd: RawFd, addr: SocketAddr) -> std::io::Result<()>
);

impl_op!(
  !detach
  "Accepts a connection on a listening socket.",
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
  Accept, fn accept(fd: RawFd) -> std::io::Result<(RawFd, SocketAddr)>
);

impl_op!(
  "Marks a socket as listening for incoming connections.",
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
  Listen, fn listen(fd: RawFd, backlog: i32) -> std::io::Result<()>
);

impl_op!(
  "Connects a socket to a remote address.",
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
  Connect, fn connect(fd: RawFd, addr: SocketAddr) -> std::io::Result<()>
);

#[cfg(feature = "buf")]
impl_op!(@impl_fn {
    "Sends data on a connected socket.",Send,send,[fd:RawFd,buf:Vec<u8> ,flags:Option<i32>],BufResult<i32,Vec<u8>> ,""
}#[doc = r" # Examples"]#[doc = r""]#[doc = r" ```rust"]#[doc = r" async fn send_example() -> std::io::Result<()> {"]#[doc = r"     # let fd = 0;"]#[doc = r#"     let data = b"Hello, server!".to_vec();"#]#[doc = r"     let (bytes_sent, _buf) = lio::send_with_buf(fd, data, None).await;"]#[doc = r#"     println!("Sent {} bytes", bytes_sent?);"#]#[doc = r"     Ok(())"]#[doc = r" }"]#[doc = r" ```"]{
    pub fn send(fd: RawFd, flags:Option<i32>)-> OperationProgress<Send<LentBuf<'static>>> {
        crate::send_with_buf(fd, Driver::get().try_lend_buf().expect("Couldn't lend driver buffer"), flags)
    }
});

impl_op!(@impl_fn {
    "Sends data on a connected socket.",Send,send,[fd:RawFd,buf:Vec<u8> ,flags:Option<i32>],BufResult<i32,Vec<u8>> ,""
}#[doc = r" # Examples"]#[doc = r""]#[doc = r" ```rust"]#[doc = r" async fn send_example() -> std::io::Result<()> {"]#[doc = r"     # let fd = 0;"]#[doc = r#"     let data = b"Hello, server!".to_vec();"#]#[doc = r"     let (bytes_sent, _buf) = lio::send_with_buf(fd, data, None).await;"]#[doc = r#"     println!("Sent {} bytes", bytes_sent?);"#]#[doc = r"     Ok(())"]#[doc = r" }"]#[doc = r" ```"]{
    pub fn send_with_buf<B>(fd: RawFd, buf: B, flags: Option<i32>)->OperationProgress<Send<B>> where B: BufLike {
        Driver::submit(Send::new(fd, Buf::new(buf),flags))
    }
});

impl_op!(@impl_fn {
    "Receives data from a connected socket.",Recv,recv,[fd:RawFd,buf:Vec<u8> ,flags:Option<i32>],BufResult<i32,Vec<u8>> ,""
}#[doc = r" # Examples"]#[doc = r""]#[doc = r" ```rust"]#[doc = r" async fn recv_example() -> std::io::Result<()> {"]#[doc = r"     # let fd = 0;"]#[doc = r"     let mut buffer = vec![0u8; 1024];"]#[doc = r"     let (res_bytes_received, buf) = lio::recv_with_buf(fd, buffer, None).await;"]#[doc = r"     let bytes_received = res_bytes_received?;"]#[doc = r#"     println!("Received {} bytes: {:?}", bytes_received, &buf[..bytes_received as usize]);"#]#[doc = r"     Ok(())"]#[doc = r" }"]#[doc = r" ```"]{
    pub fn recv_with_buf<B>(fd:RawFd,buf: B,flags:Option<i32>)->OperationProgress<Recv<B>> where B: BufLike {
        Driver::submit(Recv::new(fd,Buf::new(buf),flags))
    }
});

#[cfg(feature = "buf")]
impl_op!(@impl_fn {
    "Receives data from a connected socket.",Recv,recv,[fd:RawFd,buf:Vec<u8> ,flags:Option<i32>],BufResult<i32,Vec<u8>> ,""
}#[doc = r" # Examples"]#[doc = r""]#[doc = r" ```rust"]#[doc = r" async fn recv_example() -> std::io::Result<()> {"]#[doc = r"     # let fd = 0;"]#[doc = r"     let mut buffer = vec![0u8; 1024];"]#[doc = r"     let (res_bytes_received, buf) = lio::recv_with_buf(fd, buffer, None).await;"]#[doc = r"     let bytes_received = res_bytes_received?;"]#[doc = r#"     println!("Received {} bytes: {:?}", bytes_received, &buf[..bytes_received as usize]);"#]#[doc = r"     Ok(())"]#[doc = r" }"]#[doc = r" ```"]{
    pub fn recv(fd:RawFd,flags:Option<i32>)->OperationProgress<Recv<LentBuf<'static>>> {
        crate::recv_with_buf(fd, Driver::get().try_lend_buf().expect("Couldn't lend buffer"), flags)
    }
});

impl_op!(
  "Closes a file descriptor.",
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
  Close, fn close(fd: RawFd) -> io::Result<()>
);

impl_op!(
  "Opens a file relative to a directory file descriptor.",
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
  OpenAt, fn openat(fd: RawFd, path: CString, flags: i32) -> std::io::Result<i32>
);

#[cfg(linux)]
#[cfg_attr(docsrs, doc(cfg(linux)))]
impl_op!(
  "Copies data between file descriptors without copying to userspace (Linux only).",
  /// This operation is only available on Linux systems with io_uring support.
  /// It's equivalent to the `tee(2)` system call.
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
  Tee, fn tee(fd_in: RawFd, fd_out: RawFd, size: u32) -> std::io::Result<()>
);

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

/// Spawns a background worker thread that continuously processes I/O operations.
///
/// The worker thread will automatically process pending I/O operations, so you don't
/// need to manually call `tick()`. This is useful for applications that want async
/// operations to complete in the background.
///
/// # Example
///
/// ```no_run
/// lio::init();
/// lio::start().expect("Failed to spawn worker");
///
/// // Operations now complete automatically in the background
/// // No need to call tick() manually
///
/// lio::stop(); // Clean shutdown
/// ```
///
/// # Errors
///
/// Returns an error if a worker thread is already running.
pub fn start() -> Result<(), &'static str> {
  Driver::get().spawn_worker()
}

/// Stops the background worker thread gracefully.
///
/// Sends a shutdown signal to the worker and waits for it to finish.
/// Does nothing if no worker is running.
///
/// This should be called before `exit()` if you've spawned a worker thread.
pub fn stop() {
  Driver::get().stop_worker()
}

/// Deallocates the lio I/O driver, freeing all resources.
///
/// This must be called after `exit()` to properly clean up the driver.
/// Calling this before `exit()` or when the driver is not initialized will panic.
pub fn exit() {
  Driver::exit()
}

pub fn try_init() -> Result<(), TryInitError> {
  Driver::try_init()
}

pub fn init() {
  Driver::try_init().expect("lio is already initialised.");
}
