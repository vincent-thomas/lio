//! Async networking primitives for lio.
//!
//! This module provides high-level abstractions for network I/O operations using lio's
//! async runtime. It includes both low-level socket primitives and higher-level TCP
//! abstractions.
//!
//! # Main Types
//!
//! - [`Socket`]: Low-level async socket wrapper that provides direct access to socket operations
//! - [`TcpListener`]: High-level TCP server for accepting incoming connections
//! - [`TcpSocket`]: High-level TCP client/server connection for sending and receiving data
//!
//! # Features
//!
//! - **Async-first design**: All I/O operations return [`Progress<T>`](crate::api::progress::Progress)
//!   which can be awaited
//! - **Efficient buffer management**: Operations take ownership of buffers and return them,
//!   avoiding unnecessary copies
//! - **Zero-cost abstractions**: High-level types like `TcpListener` and `TcpSocket` are
//!   thin wrappers around `Socket` with no runtime overhead
//! - **Platform-native**: Uses the most efficient I/O mechanism available on the platform
//!   (io_uring on Linux, kqueue on BSD/macOS, etc.)
//!
//! # Examples
//!
//! ## TCP Echo Server
//!
//! ```rust,no_run
//! use lio::net::TcpListener;
//!
//! # lio::init();
//! async fn echo_server() -> std::io::Result<()> {
//!     let listener = TcpListener::bind_async("127.0.0.1:8080").await?;
//!     println!("Server listening on 127.0.0.1:8080");
//!
//!     loop {
//!         let (socket, addr) = listener.accept().await?;
//!         println!("New connection from: {}", addr);
//!
//!         // Echo data back to the client
//!         let buffer = vec![0u8; 1024];
//!         let (buffer, bytes_read) = socket.recv(buffer).await?;
//!
//!         if bytes_read > 0 {
//!             let (_, bytes_sent) = socket.send(buffer[..bytes_read].to_vec()).await?;
//!             println!("Echoed {} bytes", bytes_sent);
//!         }
//!     }
//! }
//! # lio::exit();
//! ```
//!
//! ## TCP Client
//!
//! ```rust,no_run
//! use lio::net::TcpSocket;
//! use std::net::SocketAddr;
//!
//! # lio::init();
//! async fn send_message() -> std::io::Result<()> {
//!     // Connect to server
//!     let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
//!     let socket = TcpSocket::connect_async(addr).await?;
//!
//!     // Send a message
//!     let message = b"Hello, server!".to_vec();
//!     let (message, bytes_sent) = socket.send(message).await?;
//!     println!("Sent {} bytes", bytes_sent);
//!
//!     // Receive response
//!     let buffer = vec![0u8; 1024];
//!     let (buffer, bytes_read) = socket.recv(buffer).await?;
//!     println!("Received: {:?}", &buffer[..bytes_read]);
//!
//!     // Gracefully shutdown
//!     socket.shutdown(libc::SHUT_RDWR).await?;
//!
//!     Ok(())
//! }
//! # lio::exit();
//! ```
//!
//! ## Low-Level Socket Operations
//!
//! For advanced use cases, you can use [`Socket`] directly:
//!
//! ```rust,no_run
//! use lio::net::Socket;
//! use std::net::SocketAddr;
//!
//! # lio::init();
//! async fn custom_socket() -> std::io::Result<()> {
//!     // Create a raw TCP socket
//!     let socket = Socket::new(libc::AF_INET, libc::SOCK_STREAM, 0).await?;
//!
//!     // Bind to an address
//!     let addr: SocketAddr = "0.0.0.0:9000".parse().unwrap();
//!     socket.bind(addr).await?;
//!
//!     // Listen for connections
//!     socket.listen().await?;
//!
//!     // Accept a connection (returns Socket and address)
//!     let (client_socket, client_addr) = socket.accept().await?;
//!     println!("Client connected: {}", client_addr);
//!
//!     Ok(())
//! }
//! # lio::exit();
//! ```
//!
//! # Synchronous vs Asynchronous
//!
//! Most operations in this module are async by default. However, some types like
//! [`TcpListener`] and [`TcpSocket`] provide `_sync` variants for blocking operations:
//!
//! ```rust,no_run
//! use lio::net::TcpListener;
//!
//! # lio::init();
//! fn sync_example() -> std::io::Result<()> {
//!     // This blocks until the listener is ready
//!     let listener = TcpListener::bind_sync("127.0.0.1:8080")?;
//!
//!     // Accept is still async
//!     Ok(())
//! }
//! # lio::exit();
//! ```
//!
//! # See Also
//!
//! - [`crate::api::resource`]: Resource management for file descriptors
//! - [`crate::api::progress`]: Progress type for async operations

mod socket;
mod tcp;

pub use socket::*;
pub use tcp::*;
pub mod ops;
