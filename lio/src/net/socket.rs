use std::net::SocketAddr;

use crate::{
  api::{
    self,
    io::Io,
    ops::{Bind, Connect, Listen, Recv, Send, Shutdown},
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
///     let (result, buffer) = client_socket.recv(buffer).await;
///     let bytes_read = result? as usize;
///
///     Ok(())
/// }
/// ```
///
/// ## Creating a TCP client
///
/// ```rust,no_run
/// use std::net::SocketAddr;
/// use lio::net::Socket;
///
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
///     let (result, data) = socket.send(data).await;
///     let bytes_sent = result? as usize;
///
///     // Shutdown the write side
///     socket.shutdown(libc::SHUT_WR).await?;
///
///     Ok(())
/// }
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
  /// async fn example() -> std::io::Result<()> {
  ///     // Create a TCP socket for IPv4
  ///     let socket = Socket::new(libc::AF_INET, libc::SOCK_STREAM, 0).await?;
  ///
  ///     // Create a UDP socket for IPv6
  ///     let udp_socket = Socket::new(libc::AF_INET6, libc::SOCK_DGRAM, 0).await?;
  ///
  ///     Ok(())
  /// }
  /// ```
  #[allow(clippy::new_ret_no_self)]
  pub fn new(
    domain: libc::c_int,
    ty: libc::c_int,
    proto: libc::c_int,
  ) -> Io<SocketNew> {
    let socket_accept_op = SocketNew::new(domain, ty, proto);
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
  /// async fn example() -> std::io::Result<()> {
  ///     let socket = Socket::new(libc::AF_INET, libc::SOCK_STREAM, 0).await?;
  ///
  ///     let addr: SocketAddr = "0.0.0.0:8080".parse().unwrap();
  ///     socket.bind(addr).await?;
  ///
  ///     Ok(())
  /// }
  /// ```
  pub fn bind(&self, addr: SocketAddr) -> Io<Bind> {
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
  /// async fn example() -> std::io::Result<()> {
  ///     let socket = Socket::new(libc::AF_INET, libc::SOCK_STREAM, 0).await?;
  ///
  ///     let addr: SocketAddr = "0.0.0.0:8080".parse().unwrap();
  ///     socket.bind(addr).await?;
  ///     socket.listen().await?;
  ///
  ///     Ok(())
  /// }
  /// ```
  pub fn listen(&self) -> Io<Listen> {
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
  /// ```
  pub fn accept(&self) -> Io<SocketAccept> {
    let socket_accept_op = SocketAccept::new(self.0.clone());
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
  /// ```
  pub fn connect(&self, addr: SocketAddr) -> Io<Connect> {
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
  /// async fn example(socket: Socket) -> std::io::Result<()> {
  ///     let buffer = vec![0u8; 1024];
  ///     let (result, buffer) = socket.recv(buffer).await;
  ///     let bytes_read = result? as usize;
  ///
  ///     println!("Received {} bytes", bytes_read);
  ///     println!("Data: {:?}", &buffer[..bytes_read]);
  ///
  ///     Ok(())
  /// }
  /// ```
  pub fn recv(&self, vec: Vec<u8>) -> Io<Recv<Vec<u8>>> {
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
  /// async fn example(socket: Socket) -> std::io::Result<()> {
  ///     let data = b"Hello, world!".to_vec();
  ///     let (result, data) = socket.send(data).await;
  ///     let bytes_sent = result? as usize;
  ///
  ///     println!("Sent {} bytes", bytes_sent);
  ///
  ///     Ok(())
  /// }
  /// ```
  pub fn send(&self, vec: Vec<u8>) -> Io<Send<Vec<u8>>> {
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
  /// ```
  pub fn shutdown(&self, how: i32) -> Io<Shutdown> {
    api::shutdown(&self.0, how)
  }

  /// Returns the local address this socket is bound to.
  pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
    use std::os::fd::AsRawFd;

    let fd = self.0.as_raw_fd();
    // SAFETY: sockaddr_storage is a C struct safe to zero-initialize
    let mut storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let mut len =
      std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;

    // SAFETY: fd is valid, storage/len are valid pointers with correct size
    let ret = unsafe {
      libc::getsockname(
        fd,
        &mut storage as *mut _ as *mut libc::sockaddr,
        &mut len,
      )
    };

    if ret < 0 {
      return Err(std::io::Error::last_os_error());
    }

    // Convert sockaddr_storage to SocketAddr
    match storage.ss_family as libc::c_int {
      libc::AF_INET => {
        // SAFETY: ss_family == AF_INET guarantees storage contains sockaddr_in
        let addr: &libc::sockaddr_in =
          unsafe { &*(&storage as *const _ as *const _) };
        let ip = std::net::Ipv4Addr::from(u32::from_be(addr.sin_addr.s_addr));
        let port = u16::from_be(addr.sin_port);
        Ok(SocketAddr::from((ip, port)))
      }
      libc::AF_INET6 => {
        // SAFETY: ss_family == AF_INET6 guarantees storage contains sockaddr_in6
        let addr: &libc::sockaddr_in6 =
          unsafe { &*(&storage as *const _ as *const _) };
        let ip = std::net::Ipv6Addr::from(addr.sin6_addr.s6_addr);
        let port = u16::from_be(addr.sin6_port);
        Ok(SocketAddr::from((ip, port)))
      }
      _ => Err(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "unsupported address family",
      )),
    }
  }
}
