use std::{io, net::SocketAddr};

use crate::{
  api::{
    io::Io,
    ops::{Recv, Send, Shutdown},
    resource::{AsResource, FromResource, IntoResource, Resource},
  },
  buf,
  net::ops::{UdpBindSocket, UdpConnectSocket},
};

use super::socket::Socket;

/// A UDP socket.
///
/// `UdpSocket` is a thin high-level wrapper around [`Socket`] configured for
/// datagram transport.
pub struct UdpSocket(Socket);

impl IntoResource for UdpSocket {
  fn into_resource(self) -> Resource {
    self.0.into_resource()
  }
}

impl AsResource for UdpSocket {
  fn as_resource(&self) -> &Resource {
    self.0.as_resource()
  }
}

impl FromResource for UdpSocket {
  fn from_resource(resource: Resource) -> Self {
    Self(Socket::from_resource(resource))
  }
}

impl UdpSocket {
  /// Creates a UDP socket bound to the specified local address.
  pub fn bind(addr: SocketAddr) -> Io<UdpBindSocket> {
    Io::from_op(UdpBindSocket::new(addr))
  }

  /// Creates a UDP socket and connects it to the specified remote address.
  pub fn connect(addr: SocketAddr) -> Io<UdpConnectSocket> {
    Io::from_op(UdpConnectSocket::new(addr))
  }

  /// Receives one datagram into the provided buffer.
  pub fn recv<V>(&self, vec: V) -> Io<Recv<V>>
  where
    V: buf::IoBufMutVec,
  {
    self.0.recv(vec)
  }

  /// Sends one datagram from the provided buffer.
  pub fn send<V>(&self, vec: V) -> Io<Send<V>>
  where
    V: buf::IoBufVec,
  {
    self.0.send(vec)
  }

  /// Shuts down part or all of the socket.
  pub fn shutdown(&self, how: i32) -> Io<Shutdown> {
    self.0.shutdown(how)
  }

  /// Returns the local address this socket is bound to.
  pub fn local_addr(&self) -> io::Result<SocketAddr> {
    self.0.local_addr()
  }
}
