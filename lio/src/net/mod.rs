//! Async networking primitives for lio.
//!
//! This module provides high-level abstractions for network I/O operations using lio's
//! async runtime. It includes both low-level socket primitives and higher-level TCP
//! and UDP abstractions.
//!
//! # Overview
//!
//! lio provides a unified async networking API that works with `io_uring` on Linux,
//! `kqueue` on BSD/macOS, and similar mechanisms on other platforms. This module
//! exposes:
//!
//! - [`Socket`]: Low-level async socket wrapper for socket-level operations
//! - [`TcpListener`]: High-level TCP server for accepting incoming connections
//! - [`TcpSocket`]: High-level TCP client/server connection
//! - [`UdpSocket`]: High-level UDP socket for datagram send/receive
//! - [`ops`]: Low-level async operations (advanced users only)
//!
//! # Async Design
//!
//! All I/O operations return [`Io<T>`](crate::api::io::Io) which can be awaited.
//! For example:
//!
//! ```rust,ignore
//! use lio::net::{TcpSocket, TcpListener};
//!
//! async fn example() -> std::io::Result<()> {
//!     let addr: std::net::SocketAddr = "127.0.0.1:8080".parse().unwrap();
//!
//!     let socket = TcpSocket::connect(addr).await?;
//!     let data = b"Hello, world!".to_vec();
//!     let (result, data) = socket.send(data).await;
//!     result?;
//!
//!     let buffer = vec![0u8; 1024];
//!     let (result, buffer) = socket.recv(buffer).await;
//!     result?;
//!
//!     Ok(())
//! }
//! ```
//!
//! # Efficient Buffer Management
//!
//! Operations take ownership of buffers and return them on completion,
//! avoiding unnecessary copies:
//!
//! ```rust,ignore
//! let buffer = vec![0u8; 1024];
//!
//! // Send data (buffer is consumed and returned)
//! let (result, buffer) = socket.send(buffer).await;
//! let bytes_sent = result? as usize;
//!
//! // Buffer can be reused for another operation
//! let buffer2 = b"Another message!".to_vec();
//! let (result2, buffer2) = socket.send(buffer2).await;
//! ```
//!
//! # High-Performance I/O
//!
//! Uses the most efficient I/O mechanism available on the platform:
//! - **Linux**: Uses `io_uring` for non-blocking I/O
//! - **BSD/macOS**: Uses `kqueue` for event-driven I/O
//! - **Windows**: Uses overlapped I/O
//!
//! # Socket Address Types
//!
//! All socket-related types work with [`std::net::SocketAddr`], supporting both:
//! - IPv4 addresses (`std::net::Ipv4Addr`)
//! - IPv6 addresses (`std::net::Ipv6Addr`)
//!
//! # Unix Domain Sockets
//!
//! Unix domain sockets are supported on Unix-like platforms via the `unix`
//! feature. See the [`unix`](unix) module for details.
//!
//! # See Also
//!
//! - [`crate::api::resource`]: Resource management for file descriptors
//! - [`crate::api::io`]: Io type for async operations

mod ops;
mod socket;
mod tcp;
mod udp;

pub use socket::*;
pub use tcp::*;
pub use udp::*;
