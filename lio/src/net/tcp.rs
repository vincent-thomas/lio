use std::{io, net::SocketAddr};

use crate::{
  api::{
    io::Io,
    ops::{self, Recv, Shutdown},
    resource::{AsResource, FromResource, IntoResource, Resource},
  },
  buf,
  net::ops::{TcpAccept, TcpBindListener, TcpStreamConnect},
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
/// use std::net::SocketAddr;
///
/// async fn example() -> std::io::Result<()> {
///     // Bind to an address and start listening
///     let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
///     let listener = TcpListener::bind(addr).await?;
///
///     // Accept incoming connections
///     loop {
///         let (socket, addr) = listener.accept().await?;
///         println!("New connection from: {}", addr);
///
///         // Handle the connection...
///     }
/// }
/// ```
///
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
  /// Creates a new `TcpListener` bound to the specified address.
  ///
  /// The returned listener is ready for accepting connections. This method creates a TCP
  /// socket, binds it to the provided address, and starts listening for incoming connections.
  ///
  /// Binding with a port number of 0 will request that the OS assign a port to this listener.
  /// The port allocated can be queried via the underlying socket's methods.
  ///
  /// # Examples
  ///
  /// ```rust,no_run
  /// use lio::net::TcpListener;
  /// use std::net::SocketAddr;
  ///
  /// async fn example() -> std::io::Result<()> {
  ///     // Bind using a SocketAddr
  ///     let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
  ///     let listener = TcpListener::bind(addr).await?;
  ///
  ///     // Bind to any available port
  ///     let addr: SocketAddr = "0.0.0.0:0".parse().unwrap();
  ///     let listener = TcpListener::bind(addr).await?;
  ///
  ///     Ok(())
  /// }
  /// ```
  pub fn bind(addr: SocketAddr) -> Io<TcpBindListener> {
    Io::from_op(TcpBindListener::new(addr))
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
  /// use std::net::SocketAddr;
  ///
  /// async fn example() -> std::io::Result<()> {
  ///     let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
  ///     let listener = TcpListener::bind(addr).await?;
  ///
  ///     loop {
  ///         let (socket, addr) = listener.accept().await?;
  ///         println!("Accepted connection from: {}", addr);
  ///
  ///         // Handle the socket...
  ///     }
  /// }
  /// ```
  pub fn accept(&self) -> Io<TcpAccept> {
    Io::from_op(TcpAccept::new(self.0.as_resource().clone()))
  }

  /// Returns the local address this listener is bound to.
  pub fn local_addr(&self) -> io::Result<SocketAddr> {
    self.0.local_addr()
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
/// async fn example() -> std::io::Result<()> {
///     // Connect to a server
///     let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
///     let socket = TcpSocket::connect(addr).await?;
///
///     // Send data
///     let data = b"Hello, server!".to_vec();
///     let (result, data) = socket.send(data).await;
///     let bytes_sent = result? as usize;
///
///     // Receive response
///     let buffer = vec![0u8; 1024];
///     let (result, buffer) = socket.recv(buffer).await;
///     let bytes_read = result? as usize;
///
///     Ok(())
/// }
/// ```
///
/// ## Handling an accepted connection
///
/// ```rust,no_run
/// use lio::net::TcpListener;
/// use std::net::SocketAddr;
///
/// async fn example() -> std::io::Result<()> {
///     let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
///     let listener = TcpListener::bind(addr).await?;
///
///     let (socket, addr) = listener.accept().await?;
///     println!("Connection from: {}", addr);
///
///     // Use the socket...
///     let buffer = vec![0u8; 1024];
///     let (result, buffer) = socket.recv(buffer).await;
///     let bytes_read = result? as usize;
///
///     Ok(())
/// }
/// ```
pub struct TcpStream(Socket);

impl IntoResource for TcpStream {
  fn into_resource(self) -> Resource {
    self.0.into_resource()
  }
}

impl AsResource for TcpStream {
  fn as_resource(&self) -> &Resource {
    self.0.as_resource()
  }
}

impl FromResource for TcpStream {
  fn from_resource(resource: Resource) -> Self {
    Self(Socket::from_resource(resource))
  }
}

impl TcpStream {
  /// Opens a TCP connection to a remote host.
  ///
  /// This method creates a new TCP socket and connects it to the specified remote address.
  /// The connection is established asynchronously, and the method returns once the
  /// connection is ready to use.
  ///
  /// # Examples
  ///
  /// ```rust,no_run
  /// use std::net::SocketAddr;
  /// use lio::net::TcpStream;
  ///
  /// async fn example() -> std::io::Result<()> {
  ///     let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
  ///     let socket = TcpStream::connect(addr).await?;
  ///
  ///     println!("Connected to server");
  ///
  ///     Ok(())
  /// }
  /// ```
  pub fn connect(addr: SocketAddr) -> Io<TcpStreamConnect> {
    Io::from_op(TcpStreamConnect::new(addr))
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
  /// async fn example() -> std::io::Result<()> {
  ///     let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
  ///     let socket = TcpSocket::connect(addr).await?;
  ///
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
  pub fn recv<V>(&self, vec: V) -> Io<Recv<V>>
  where
    V: buf::IoBufMutVec + std::marker::Send + Sync + 'static,
  {
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
  /// async fn example() -> std::io::Result<()> {
  ///     let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
  ///     let socket = TcpSocket::connect(addr).await?;
  ///
  ///     let data = b"Hello, server!".to_vec();
  ///     let (result, data) = socket.send(data).await;
  ///     let bytes_sent = result? as usize;
  ///
  ///     println!("Sent {} bytes", bytes_sent);
  ///
  ///     Ok(())
  /// }
  /// ```
  pub fn send<V>(&self, vec: V) -> Io<ops::Send<V>>
  where
    V: buf::IoBufVec + std::marker::Send + Sync + 'static,
  {
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
  /// async fn example() -> std::io::Result<()> {
  ///     let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
  ///     let socket = TcpSocket::connect(addr).await?;
  ///
  ///     // Send request
  ///     let data = b"GET / HTTP/1.1\r\n\r\n".to_vec();
  ///     let (result, _) = socket.send(data).await;
  ///     result?;
  ///
  ///     // Shutdown write side to signal end of request
  ///     socket.shutdown(libc::SHUT_WR).await?;
  ///
  ///     // Can still receive the response
  ///     let buffer = vec![0u8; 4096];
  ///     let (result, buffer) = socket.recv(buffer).await;
  ///     let bytes_read = result? as usize;
  ///
  ///     Ok(())
  /// }
  /// ```
  pub fn shutdown(&self, how: i32) -> Io<Shutdown> {
    self.0.shutdown(how)
  }
}
