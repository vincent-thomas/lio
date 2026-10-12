//! Unix domain socket support.
//!
//! [`UnixListener`] and [`UnixStream`] are thin wrappers over the generic
//! socket/bind/listen/connect/accept operations. Backends translate the
//! portable address into the platform's native `sockaddr_un`, so this module
//! contains no platform-specific code.
//!
//! # Examples
//!
//! ```rust,no_run
//! use lio::net::{UnixListener, UnixStream};
//!
//! async fn echo_once() -> std::io::Result<()> {
//!     let listener = UnixListener::bind("/tmp/echo.sock")?.await?;
//!     let client = UnixStream::connect("/tmp/echo.sock")?.await?;
//!     let server = listener.accept().await?;
//!
//!     let (result, _) = client.send(b"ping".to_vec()).await;
//!     result?;
//!     let (result, buf) = server.recv(vec![0u8; 4]).await;
//!     assert_eq!(&buf[..result? as usize], b"ping");
//!     Ok(())
//! }
//! ```

use std::{io, path::Path};

use crate::{
  api::{
    ShutdownHow,
    io::Io,
    ops::{self, Recv, Shutdown},
    resource::{AsResource, FromResource, IntoResource, Resource},
  },
  backend::op::{SocketAddrBuf, unix_socket_addr_buf},
  buf,
  net::ops::{BindListener, StreamConnect, UnixAccept},
};

use super::socket::Socket;

/// Socket-create/bind/listen operation that returns a [`UnixListener`].
pub type UnixBindListener = BindListener<UnixListener>;

/// Socket-create/connect operation that returns a [`UnixStream`].
pub type UnixStreamConnect = StreamConnect<UnixStream>;

/// A Unix domain socket server, listening for connections.
pub struct UnixListener(Socket);

impl IntoResource for UnixListener {
  fn into_resource(self) -> Resource {
    self.0.into_resource()
  }
}

impl AsResource for UnixListener {
  fn as_resource(&self) -> &Resource {
    self.0.as_resource()
  }
}

impl FromResource for UnixListener {
  fn from_resource(resource: Resource) -> Self {
    Self(Socket::from_resource(resource))
  }
}

impl UnixListener {
  /// Creates a Unix stream socket bound to `path` and starts listening.
  ///
  /// The socket file is created by the bind; it fails with `AddrInUse` if
  /// something already exists at `path`.
  ///
  /// # Errors
  ///
  /// Returns `InvalidInput` right away if `path` contains a NUL byte or is too
  /// long for a socket address. Backends may reject paths that are shorter
  /// than lio's limit but longer than the platform's `sun_path`.
  pub fn bind(path: impl AsRef<Path>) -> io::Result<Io<UnixBindListener>> {
    let addr = path_to_addr(path.as_ref())?;
    Ok(Io::from_op(BindListener::unix(addr)))
  }

  /// Accepts a new incoming connection from this listener.
  pub fn accept(&self) -> Io<UnixAccept> {
    Io::from_op(UnixAccept::new(self.0.as_resource().clone()))
  }
}

/// A connected Unix domain stream socket.
pub struct UnixStream(Socket);

impl IntoResource for UnixStream {
  fn into_resource(self) -> Resource {
    self.0.into_resource()
  }
}

impl AsResource for UnixStream {
  fn as_resource(&self) -> &Resource {
    self.0.as_resource()
  }
}

impl FromResource for UnixStream {
  fn from_resource(resource: Resource) -> Self {
    Self(Socket::from_resource(resource))
  }
}

impl UnixStream {
  /// Connects a new Unix stream socket to the listener at `path`.
  ///
  /// # Errors
  ///
  /// Returns `InvalidInput` right away if `path` contains a NUL byte or is too
  /// long for a socket address.
  pub fn connect(path: impl AsRef<Path>) -> io::Result<Io<UnixStreamConnect>> {
    let addr = path_to_addr(path.as_ref())?;
    Ok(Io::from_op(StreamConnect::unix(addr)))
  }

  /// Receives data from the stream into the provided buffer.
  pub fn recv<V>(&self, vec: V) -> Io<Recv<V>>
  where
    V: buf::IoBufMutVec + std::marker::Send + Sync + 'static,
  {
    self.0.recv(vec)
  }

  /// Sends data through the stream.
  pub fn send<V>(&self, vec: V) -> Io<ops::Send<V>>
  where
    V: buf::IoBufVec + std::marker::Send + Sync + 'static,
  {
    self.0.send(vec)
  }

  /// Shuts down the read, write, or both halves of this connection.
  pub fn shutdown(&self, how: ShutdownHow) -> Io<Shutdown> {
    self.0.shutdown(how)
  }
}

fn path_to_addr(path: &Path) -> io::Result<SocketAddrBuf> {
  let bytes = path.as_os_str().as_encoded_bytes();
  if bytes.contains(&0) {
    return Err(io::Error::new(
      io::ErrorKind::InvalidInput,
      "Unix socket path contains a NUL byte",
    ));
  }
  unix_socket_addr_buf(bytes)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn path_to_addr_rejects_nul_and_oversized_paths() {
    let err = path_to_addr(Path::new("a\0b")).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);

    let long = "x".repeat(200);
    let err = path_to_addr(Path::new(&long)).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
  }

  #[test]
  fn path_to_addr_keeps_path_bytes() {
    let addr = path_to_addr(Path::new("/tmp/lio.sock")).unwrap();
    assert_eq!(
      &addr.unix_path[..addr.unix_path_len as usize],
      b"/tmp/lio.sock"
    );
  }
}
