//! I/O backend implementations for lio.
//!
//! This module provides the abstraction layer for different I/O backends and their
//! implementations. It defines the core trait [`IoBackend`] that all backends must
//! implement and manages the lifecycle of I/O operations.
//!
//! # Architecture
//!
//! The backend system is designed for thread-per-core runtime integration:
//!
//! - **[`IoBackend`]**: Unified trait for submission and completion. Each platform has
//!   specific implementations (io_uring on Linux, kqueue on macOS/BSD, IOCP on Windows).
//!
//! # Design Goals
//!
//! - **Zero runtime allocation**: All capacity is pre-allocated via [`init`](IoBackend::init)
//! - **Dyn-compatible**: Backends can be used as `Box<dyn IoBackend>`
//! - **Thread-local ownership**: Each thread owns its backend instance (`&mut self`)
//! - **Batched submission**: a backend starts each op as it is pushed, and
//!   hands the kernel what has to wait in its next `wait`
//! - **One table**: [`Tracked`] owns every operation from push to report. A
//!   backend holds keys, never the right to report
//!
//! # Example
//!
//! ```
//! use lio::backend::{Tracked, op::BorrowedOp, impls::DefaultBackend};
//! use std::time::Duration;
//!
//! // A backend is driven through the table that tracks its operations.
//! // `Lio` owns one; this is the same thing by hand.
//! let mut io = Tracked::new(DefaultBackend::new());
//! io.init(1024).unwrap();
//!
//! // A nop borrows nothing. Real submissions borrow an operation's memory,
//! // which the driver keeps in place until the operation is reported.
//! // SAFETY: this nop has no buffer for the backend to retain.
//! unsafe { io.push(1, BorrowedOp::nop()) };
//!
//! let mut completions = Vec::new();
//! io.wait(Some(Duration::ZERO), &mut completions).unwrap();
//! ```

pub mod ds;
pub mod op;
pub mod testing;

/// Test utilities for IoBackend implementations.
///
/// Use the `test_io_backend!` macro to generate a comprehensive test suite
/// for your IoBackend implementation.
#[macro_use]
pub mod test_macro;

#[cfg(feature = "backend_impls")]
pub mod impls {
  #[cfg(unix)]
  #[macro_use]
  pub(crate) mod syscall;

  #[cfg(target_os = "linux")]
  mod io_uring;

  #[cfg(target_os = "linux")]
  pub use io_uring::IoUring;

  #[cfg(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "ios",
    target_os = "tvos",
    target_os = "watchos",
    target_os = "freebsd",
    target_os = "dragonfly",
    target_os = "openbsd",
    target_os = "netbsd"
  ))]
  mod pollingv2;
  #[cfg(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "ios",
    target_os = "tvos",
    target_os = "watchos",
    target_os = "freebsd",
    target_os = "dragonfly",
    target_os = "openbsd",
    target_os = "netbsd"
  ))]
  pub use pollingv2::Poller;

  #[cfg(windows)]
  mod iocp;
  #[cfg(windows)]
  pub use iocp::*;

  #[cfg(target_os = "wasi")]
  pub(crate) mod wasi;
  #[cfg(target_os = "wasi")]
  pub use wasi::Wasi;

  #[cfg(any(unix, windows))]
  pub(crate) use super::sockaddr;
  #[cfg(unix)]
  pub(crate) mod iovec;
  #[cfg(unix)]
  pub(crate) mod unix_stat;

  /// The backend [`Lio::new`](crate::Lio::new) uses on this platform.
  #[cfg(target_os = "linux")]
  pub type DefaultBackend = IoUring;

  /// The backend [`Lio::new`](crate::Lio::new) uses on this platform.
  #[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "tvos",
    target_os = "watchos",
    target_os = "freebsd",
    target_os = "dragonfly",
    target_os = "openbsd",
    target_os = "netbsd"
  ))]
  pub type DefaultBackend = Poller;

  /// The backend [`Lio::new`](crate::Lio::new) uses on this platform.
  #[cfg(windows)]
  pub type DefaultBackend = Iocp;

  /// The backend [`Lio::new`](crate::Lio::new) uses on this platform.
  #[cfg(target_os = "wasi")]
  pub type DefaultBackend = Wasi;
}

pub(crate) mod store;

/// The host OS's native close and standard handles.
#[path = "impls/native.rs"]
pub(crate) mod native;

/// OS socket-address conversions. Lives with the backend implementations but
/// is also needed by `net` when no backend implementations are compiled in.
#[cfg(any(unix, windows))]
#[path = "impls/sockaddr.rs"]
#[cfg_attr(not(feature = "backend_impls"), allow(dead_code))]
pub(crate) mod sockaddr;

/// Socket-address queries on WASI, where a socket is a typed handle held by
/// the WASI backend instead of a descriptor the OS can be asked about.
#[cfg(target_os = "wasi")]
pub(crate) mod sockaddr {
  /// Looks up the local (`peer == false`) or remote address of a socket.
  #[cfg(feature = "high")]
  pub(crate) fn socket_name(
    socket: &crate::api::resource::Resource,
    peer: bool,
  ) -> std::io::Result<std::net::SocketAddr> {
    #[cfg(feature = "backend_impls")]
    {
      use std::os::fd::AsRawFd;
      super::impls::wasi::socket_name(socket.as_raw_fd(), peer)
    }
    // Without the WASI backend nothing creates sockets.
    #[cfg(not(feature = "backend_impls"))]
    {
      let _ = (socket, peer);
      Err(std::io::Error::from(std::io::ErrorKind::Unsupported))
    }
  }
}

mod table;

pub(crate) use table::Keep;
pub use table::{
  Done, IoBackend, OpCompleted, OpKey, PerOp, Queued, Started, Tracked,
};
