use std::{
  io,
  net::{SocketAddr, ToSocketAddrs},
};

use crate::{
  api::{
    self,
    ops::{self, Accept, Recv, Shutdown},
    progress::Progress,
    resource::{AsResource, FromResource, IntoResource, Resource},
  },
  net::ops::TcpAccept,
  operation::{OpMeta, Operation, OperationExt},
};

use super::socket::Socket;

/// A TCP socket server, listening for connections.
///
/// `TcpListener` provides a high-level interface for creating TCP servers. After being
/// created by binding to a socket address, it listens for and accepts incoming TCP
/// connections.
///
/// # Examples
///
/// ## Basic server
///
/// ```rust,no_run
/// use lio::net::TcpListener;
///
/// # lio::init();
/// async fn example() -> std::io::Result<()> {
///     // Bind to an address and start listening
///     let listener = TcpListener::bind_async("127.0.0.1:8080").await?;
///
///     // Accept incoming connections
///     loop {
///         let (socket, addr) = listener.accept().await?;
///         println!("New connection from: {}", addr);
///
///         // Handle the connection...
///     }
/// }
/// # lio::exit();
/// ```
///
/// ## Synchronous binding
///
/// ```rust,no_run
/// use lio::net::TcpListener;
///
/// # lio::init();
/// fn example() -> std::io::Result<()> {
///     // Bind synchronously (blocks until ready)
///     let listener = TcpListener::bind_sync("127.0.0.1:8080")?;
///
///     // Accept connections asynchronously
///     Ok(())
/// }
/// # lio::exit();
/// ```
pub struct TcpListener(Socket);

impl IntoResource for TcpListener {
  fn into_resource(self) -> Resource {
    self.0.into_resource()
  }
}

impl FromResource for TcpListener {
  fn from_resource(resource: Resource) -> Self {
    Self(Socket::from_resource(resource))
  }
}

impl TcpListener {
  /// Creates a new `TcpListener` which will be bound to the specified address asynchronously.
  ///
  /// The returned listener is ready for accepting connections. This method creates a TCP
  /// socket, binds it to the provided address, and starts listening for incoming connections.
  ///
  /// Binding with a port number of 0 will request that the OS assign a port to this listener.
  /// The port allocated can be queried via the underlying socket's methods.
  ///
  /// The address type can be any implementor of [`ToSocketAddrs`], including string slices
  /// and tuples of IP address and port.
  ///
  /// # Examples
  ///
  /// ```rust,no_run
  /// use lio::net::TcpListener;
  ///
  /// # lio::init();
  /// async fn example() -> std::io::Result<()> {
  ///     // Bind using a string
  ///     let listener = TcpListener::bind_async("127.0.0.1:8080").await?;
  ///
  ///     // Bind to any available address
  ///     let listener = TcpListener::bind_async("0.0.0.0:0").await?;
  ///
  ///     Ok(())
  /// }
  /// # lio::exit();
  /// ```
  // TODO: AsyncToSocketAddrs
  pub async fn bind_async(addr: impl ToSocketAddrs) -> io::Result<Self> {
    let mut addrs = addr.to_socket_addrs()?;

    let socket = Socket::new(libc::SOCK_STREAM, libc::AF_INET, 0).await?;
    let addr = addrs.next().unwrap();
    socket.bind(addr).await?;
    socket.listen().await?;
    return Ok(TcpListener(socket));
  }

  /// Creates a new `TcpListener` which will be bound to the specified address synchronously.
  ///
  /// This is the blocking version of [`bind_async`](Self::bind_async). It will block the
  /// current thread until the socket is created, bound, and listening.
  ///
  /// # Examples
  ///
  /// ```rust,no_run
  /// use lio::net::TcpListener;
  ///
  /// # lio::init();
  /// fn example() -> std::io::Result<()> {
  ///     // This will block until the listener is ready
  ///     let listener = TcpListener::bind_sync("127.0.0.1:8080")?;
  ///
  ///     Ok(())
  /// }
  /// # lio::exit();
  /// ```
  pub fn bind_sync(addr: impl ToSocketAddrs) -> io::Result<Self> {
    let mut addrs = addr.to_socket_addrs()?;

    let socket = Socket::new(libc::SOCK_STREAM, libc::AF_INET, 0).wait()?;
    let addr = addrs.next().unwrap();
    socket.bind(addr).wait()?;
    socket.listen().wait()?;
    return Ok(TcpListener(socket));
  }

  /// Accepts a new incoming connection from this listener.
  ///
  /// This function will await until a new TCP connection is established. When a connection
  /// is established, the corresponding [`TcpSocket`] and the remote peer's address will be
  /// returned.
  ///
  /// # Returns
  ///
  /// A tuple containing:
  /// - A [`TcpSocket`] representing the accepted client connection
  /// - The [`SocketAddr`] of the connected client
  ///
  /// # Examples
  ///
  /// ```rust,no_run
  /// use lio::net::TcpListener;
  ///
  /// # lio::init();
  /// async fn example() -> std::io::Result<()> {
  ///     let listener = TcpListener::bind_async("127.0.0.1:8080").await?;
  ///
  ///     loop {
  ///         let (socket, addr) = listener.accept().await?;
  ///         println!("Accepted connection from: {}", addr);
  ///
  ///         // Handle the socket...
  ///     }
  /// }
  /// # lio::exit();
  /// ```
  pub fn accept(&self) -> Progress<'_, TcpAccept> {
    // Create Accept operation and wrap it in SocketAccept
    let socket_accept_op = TcpAccept(Accept::new(self.0.as_resource().clone()));
    Progress::from_op(socket_accept_op)
  }
}

/// A TCP socket connection.
///
/// `TcpSocket` represents an established TCP connection between a local and a remote socket.
/// It can be created by connecting to a remote address or by accepting a connection from a
/// [`TcpListener`].
///
/// `TcpSocket` provides methods for reading and writing data over the connection, as well
/// as shutting down the connection gracefully.
///
/// # Examples
///
/// ## Creating a client connection
///
/// ```rust,no_run
/// use std::net::SocketAddr;
/// use lio::net::TcpSocket;
///
/// # lio::init();
/// async fn example() -> std::io::Result<()> {
///     // Connect to a server
///     let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
///     let socket = TcpSocket::connect_async(addr).await?;
///
///     // Send data
///     let data = b"Hello, server!".to_vec();
///     let (data, bytes_sent) = socket.send(data).await?;
///
///     // Receive response
///     let buffer = vec![0u8; 1024];
///     let (buffer, bytes_read) = socket.recv(buffer).await?;
///
///     Ok(())
/// }
/// # lio::exit();
/// ```
///
/// ## Handling an accepted connection
///
/// ```rust,no_run
/// use lio::net::TcpListener;
///
/// # lio::init();
/// async fn example() -> std::io::Result<()> {
///     let listener = TcpListener::bind_async("127.0.0.1:8080").await?;
///
///     let (socket, addr) = listener.accept().await?;
///     println!("Connection from: {}", addr);
///
///     // Use the socket...
///     let buffer = vec![0u8; 1024];
///     let (buffer, bytes_read) = socket.recv(buffer).await?;
///
///     Ok(())
/// }
/// # lio::exit();
/// ```
pub struct TcpSocket(Socket);

impl IntoResource for TcpSocket {
  fn into_resource(self) -> Resource {
    self.0.into_resource()
  }
}

impl AsResource for TcpSocket {
  fn as_resource(&self) -> &Resource {
    self.0.as_resource()
  }
}

impl FromResource for TcpSocket {
  fn from_resource(resource: Resource) -> Self {
    Self(Socket::from_resource(resource))
  }
}

impl TcpSocket {
  /// Opens a TCP connection to a remote host asynchronously.
  ///
  /// This method creates a new TCP socket and connects it to the specified remote address.
  /// The connection is established asynchronously, and the method returns once the
  /// connection is ready to use.
  ///
  /// # Examples
  ///
  /// ```rust,no_run
  /// use std::net::SocketAddr;
  /// use lio::net::TcpSocket;
  ///
  /// # lio::init();
  /// async fn example() -> std::io::Result<()> {
  ///     let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
  ///     let socket = TcpSocket::connect_async(addr).await?;
  ///
  ///     println!("Connected to server");
  ///
  ///     Ok(())
  /// }
  /// # lio::exit();
  /// ```
  pub async fn connect_async(addr: SocketAddr) -> io::Result<Self> {
    let socket = Socket::new(libc::SOCK_STREAM, libc::AF_INET, 0).await?;
    api::connect(socket.as_resource(), addr).await?;
    Ok(TcpSocket(socket))
  }

  /// Opens a TCP connection to a remote host synchronously.
  ///
  /// This is the blocking version of [`connect_async`](Self::connect_async). It will block
  /// the current thread until the connection is established.
  ///
  /// # Examples
  ///
  /// ```rust,no_run
  /// use std::net::SocketAddr;
  /// use lio::net::TcpSocket;
  ///
  /// # lio::init();
  /// fn example() -> std::io::Result<()> {
  ///     let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
  ///     // This will block until connected
  ///     let socket = TcpSocket::connect_sync(addr)?;
  ///
  ///     Ok(())
  /// }
  /// # lio::exit();
  /// ```
  pub fn connect_sync(addr: SocketAddr) -> io::Result<Self> {
    let socket = Socket::new(libc::SOCK_STREAM, libc::AF_INET, 0).wait()?;
    api::connect(&socket, addr).wait()?;
    Ok(TcpSocket(socket))
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
  /// use lio::net::TcpSocket;
  /// use std::net::SocketAddr;
  ///
  /// # lio::init();
  /// async fn example() -> std::io::Result<()> {
  ///     let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
  ///     let socket = TcpSocket::connect_async(addr).await?;
  ///
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
  pub fn recv(&self, vec: Vec<u8>) -> Progress<'_, Recv<Vec<u8>>> {
    self.0.recv(vec)
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
  /// use lio::net::TcpSocket;
  /// use std::net::SocketAddr;
  ///
  /// # lio::init();
  /// async fn example() -> std::io::Result<()> {
  ///     let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
  ///     let socket = TcpSocket::connect_async(addr).await?;
  ///
  ///     let data = b"Hello, server!".to_vec();
  ///     let (data, bytes_sent) = socket.send(data).await?;
  ///
  ///     println!("Sent {} bytes", bytes_sent);
  ///
  ///     Ok(())
  /// }
  /// # lio::exit();
  /// ```
  pub fn send(&self, vec: Vec<u8>) -> Progress<'_, ops::Send<Vec<u8>>> {
    self.0.send(vec)
  }

  /// Shuts down the read, write, or both halves of this connection.
  ///
  /// This operation disables further send and/or receive operations on the socket.
  /// This is useful for implementing graceful shutdowns where you want to signal
  /// to the peer that no more data will be sent while still being able to receive data.
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
  /// use lio::net::TcpSocket;
  /// use std::net::SocketAddr;
  ///
  /// # lio::init();
  /// async fn example() -> std::io::Result<()> {
  ///     let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
  ///     let socket = TcpSocket::connect_async(addr).await?;
  ///
  ///     // Send request
  ///     let data = b"GET / HTTP/1.1\r\n\r\n".to_vec();
  ///     socket.send(data).await?;
  ///
  ///     // Shutdown write side to signal end of request
  ///     socket.shutdown(libc::SHUT_WR).await?;
  ///
  ///     // Can still receive the response
  ///     let buffer = vec![0u8; 4096];
  ///     let (buffer, bytes_read) = socket.recv(buffer).await?;
  ///
  ///     Ok(())
  /// }
  /// # lio::exit();
  /// ```
  pub fn shutdown(&self, how: i32) -> Progress<'_, Shutdown> {
    self.0.shutdown(how)
  }
}
