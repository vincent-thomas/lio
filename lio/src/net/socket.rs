use std::net::SocketAddr;

use crate::{
  api::{
    self,
    io::Io,
    ops::{self, Accept, Bind, Connect, Listen, Recv, Send, Shutdown},
    resource::{AsResource, FromResource, IntoResource, Resource},
  },
  net::ops::{SocketAccept, SocketNew},
};

/// A high-level async socket wrapper for network I/O operations.
///
/// `Socket` provides an ergonomic API for performing asynchronous socket operations
/// in lio.
///
/// # Examples
///
/// ## Creating a TCP server
///
/// ```rust,no_run
/// use std::net::SocketAddr;
/// use lio::net::Socket;
///
/// # lio::init();
/// async fn run_server() -> std::io::Result<()> {
///     // Create a new socket
///     let socket = Socket::new(libc::AF_INET, libc::SOCK_STREAM, 0).await?;
///
///     // Bind to an address
///     let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
///     socket.bind(addr).await?;
///
///     // Start listening for connections
///     socket.listen().await?;
///
///     // Accept incoming connections
///     let (client_socket, client_addr) = socket.accept().await?;
///
///     // Receive data from the client
///     let buffer = vec![0u8; 1024];
///     let (buffer, bytes_read) = client_socket.recv(buffer).await?;
///
///     Ok(())
/// }
/// # lio::exit();
/// ```
///
/// ## Creating a TCP client
///
/// ```rust,no_run
/// use std::net::SocketAddr;
/// use lio::net::Socket;
///
/// # lio::init();
/// async fn connect_to_server() -> std::io::Result<()> {
///     // Create a new socket
///     let socket = Socket::new(libc::AF_INET, libc::SOCK_STREAM, 0).await?;
///
///     // Connect to a server
///     let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
///     socket.connect(addr).await?;
///
///     // Send data
///     let data = b"Hello, server!".to_vec();
///     let (data, bytes_sent) = socket.send(data).await?;
///
///     // Shutdown the write side
///     socket.shutdown(libc::SHUT_WR).await?;
///
///     Ok(())
/// }
/// # lio::exit();
/// ```
pub struct Socket(Resource);

impl IntoResource for Socket {
  fn into_resource(self) -> Resource {
    self.0
  }
}

impl AsResource for Socket {
  fn as_resource(&self) -> &Resource {
    &self.0
  }
}

impl FromResource for Socket {
  fn from_resource(resource: Resource) -> Self {
    Self(resource)
  }
}

impl Socket {
  /// Creates a new socket with the specified domain, type, and protocol.
  ///
  /// This is a low-level operation that directly wraps the `socket(2)` system call.
  ///
  /// # Parameters
  ///
  /// - `domain`: The communication domain (e.g., `AF_INET` for IPv4, `AF_INET6` for IPv6)
  /// - `ty`: The socket type (e.g., `SOCK_STREAM` for TCP, `SOCK_DGRAM` for UDP)
  /// - `proto`: The protocol to use (typically `0` for default protocol)
  ///
  /// # Examples
  ///
  /// ```rust,no_run
  /// use lio::net::Socket;
  ///
  /// # lio::init();
  /// async fn example() -> std::io::Result<()> {
  ///     // Create a TCP socket for IPv4
  ///     let socket = Socket::new(libc::AF_INET, libc::SOCK_STREAM, 0).await?;
  ///
  ///     // Create a UDP socket for IPv6
  ///     let udp_socket = Socket::new(libc::AF_INET6, libc::SOCK_DGRAM, 0).await?;
  ///
  ///     Ok(())
  /// }
  /// # lio::exit();
  /// ```
  pub fn new(
    domain: libc::c_int,
    ty: libc::c_int,
    proto: libc::c_int,
  ) -> Io<'static, SocketNew> {
    let socket_accept_op = SocketNew(ops::Socket::new(domain, ty, proto));
    Io::from_op(socket_accept_op)
  }

  /// Binds the socket to the specified address.
  ///
  /// This assigns a local address to the socket. For server sockets, this is typically
  /// the address and port that the server will listen on.
  ///
  /// # Examples
  ///
  /// ```rust,no_run
  /// use std::net::SocketAddr;
  /// use lio::net::Socket;
  ///
  /// # lio::init();
  /// async fn example() -> std::io::Result<()> {
  ///     let socket = Socket::new(libc::AF_INET, libc::SOCK_STREAM, 0).await?;
  ///
  ///     let addr: SocketAddr = "0.0.0.0:8080".parse().unwrap();
  ///     socket.bind(addr).await?;
  ///
  ///     Ok(())
  /// }
  /// # lio::exit();
  /// ```
  pub fn bind(&self, addr: SocketAddr) -> Io<'_, Bind> {
    api::bind(&self.0, addr)
  }

  /// Marks the socket as a passive socket that will accept incoming connections.
  ///
  /// After calling this method, the socket will be ready to accept incoming connections
  /// via [`accept()`](Self::accept). The backlog is set to 128 connections.
  ///
  /// # Examples
  ///
  /// ```rust,no_run
  /// use std::net::SocketAddr;
  /// use lio::net::Socket;
  ///
  /// # lio::init();
  /// async fn example() -> std::io::Result<()> {
  ///     let socket = Socket::new(libc::AF_INET, libc::SOCK_STREAM, 0).await?;
  ///
  ///     let addr: SocketAddr = "0.0.0.0:8080".parse().unwrap();
  ///     socket.bind(addr).await?;
  ///     socket.listen().await?;
  ///
  ///     Ok(())
  /// }
  /// # lio::exit();
  /// ```
  pub fn listen(&self) -> Io<'_, Listen> {
    api::listen(&self.0, 128)
  }

  /// Accepts an incoming connection on a listening socket.
  ///
  /// This operation will block until a client connects to the socket. When a connection
  /// is established, it returns a new `Socket` representing the client connection and
  /// the client's address.
  ///
  /// # Returns
  ///
  /// A tuple containing:
  /// - A new `Socket` representing the accepted client connection
  /// - The `SocketAddr` of the connected client
  ///
  /// # Examples
  ///
  /// ```rust,no_run
  /// use std::net::SocketAddr;
  /// use lio::net::Socket;
  ///
  /// # lio::init();
  /// async fn example() -> std::io::Result<()> {
  ///     let socket = Socket::new(libc::AF_INET, libc::SOCK_STREAM, 0).await?;
  ///
  ///     let addr: SocketAddr = "0.0.0.0:8080".parse().unwrap();
  ///     socket.bind(addr).await?;
  ///     socket.listen().await?;
  ///
  ///     let (client_socket, client_addr) = socket.accept().await?;
  ///     println!("Accepted connection from: {}", client_addr);
  ///
  ///     Ok(())
  /// }
  /// # lio::exit();
  /// ```
  pub fn accept(&self) -> Io<'_, SocketAccept> {
    // Create Accept operation and wrap it in SocketAccept
    let socket_accept_op = SocketAccept(Accept::new(self.0.clone()));
    Io::from_op(socket_accept_op)
  }

  /// Initiates a connection to a remote socket at the specified address.
  ///
  /// For TCP sockets, this establishes a connection to the server. The operation
  /// completes when the connection is fully established or fails.
  ///
  /// # Examples
  ///
  /// ```rust,no_run
  /// use std::net::SocketAddr;
  /// use lio::net::Socket;
  ///
  /// # lio::init();
  /// async fn example() -> std::io::Result<()> {
  ///     let socket = Socket::new(libc::AF_INET, libc::SOCK_STREAM, 0).await?;
  ///
  ///     let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
  ///     socket.connect(addr).await?;
  ///
  ///     println!("Connected to server");
  ///
  ///     Ok(())
  /// }
  /// # lio::exit();
  /// ```
  pub fn connect(&self, addr: SocketAddr) -> Io<'_, Connect> {
    api::connect(&self.0, addr)
  }

  /// Receives data from the socket into the provided buffer.
  ///
  /// This operation reads data from the socket and returns both the buffer and the
  /// number of bytes read. The buffer is passed by value and returned, allowing for
  /// efficient async buffer management.
  ///
  /// # Returns
  ///
  /// A tuple containing:
  /// - The buffer (returned for reuse)
  /// - The number of bytes read
  ///
  /// # Examples
  ///
  /// ```rust,no_run
  /// use lio::net::Socket;
  ///
  /// # lio::init();
  /// async fn example(socket: Socket) -> std::io::Result<()> {
  ///     let buffer = vec![0u8; 1024];
  ///     let (buffer, bytes_read) = socket.recv(buffer).await?;
  ///
  ///     println!("Received {} bytes", bytes_read);
  ///     println!("Data: {:?}", &buffer[..bytes_read]);
  ///
  ///     Ok(())
  /// }
  /// # lio::exit();
  /// ```
  pub fn recv(&self, vec: Vec<u8>) -> Io<'_, Recv<Vec<u8>>> {
    api::recv(&self.0, vec, None)
  }

  /// Sends data through the socket.
  ///
  /// This operation writes data to the socket and returns both the buffer and the
  /// number of bytes sent. The buffer is passed by value and returned, allowing for
  /// efficient async buffer management.
  ///
  /// # Returns
  ///
  /// A tuple containing:
  /// - The buffer (returned for reuse)
  /// - The number of bytes sent
  ///
  /// # Examples
  ///
  /// ```rust,no_run
  /// use lio::net::Socket;
  ///
  /// # lio::init();
  /// async fn example(socket: Socket) -> std::io::Result<()> {
  ///     let data = b"Hello, world!".to_vec();
  ///     let (data, bytes_sent) = socket.send(data).await?;
  ///
  ///     println!("Sent {} bytes", bytes_sent);
  ///
  ///     Ok(())
  /// }
  /// # lio::exit();
  /// ```
  pub fn send(&self, vec: Vec<u8>) -> Io<'_, Send<Vec<u8>>> {
    api::send(&self.0, vec, None)
  }

  /// Shuts down part or all of the socket connection.
  ///
  /// This operation disables further send and/or receive operations on the socket.
  ///
  /// # Parameters
  ///
  /// - `how`: Specifies which operations to shut down:
  ///   - `SHUT_RD` (0): Further receives are disallowed
  ///   - `SHUT_WR` (1): Further sends are disallowed
  ///   - `SHUT_RDWR` (2): Further sends and receives are disallowed
  ///
  /// # Examples
  ///
  /// ```rust,no_run
  /// use lio::net::Socket;
  ///
  /// # lio::init();
  /// async fn example(socket: Socket) -> std::io::Result<()> {
  ///     // Send all data...
  ///
  ///     // Shutdown the write side to signal EOF to the peer
  ///     socket.shutdown(libc::SHUT_WR).await?;
  ///
  ///     // Can still receive data...
  ///
  ///     Ok(())
  /// }
  /// # lio::exit();
  /// ```
  pub fn shutdown(&self, how: i32) -> Io<'_, Shutdown> {
    api::shutdown(&self.0, how)
  }
}
