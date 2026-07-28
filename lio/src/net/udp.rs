use std::{io, net::SocketAddr};

use crate::{
  api::{
    ShutdownHow,
    io::Io,
    ops::{Recv, RecvFrom, Send, Shutdown},
    resource::{AsResource, FromResource, IntoResource, Resource},
  },
  buf,
  net::ops::{UdpBindSocket, UdpConnectSocket},
};

use super::socket::Socket;

/// A UDP socket.
///
/// `UdpSocket` supports both connected and unconnected datagram workflows:
///
/// - use [`bind`](Self::bind) to receive datagrams and to send with
///   [`sendto`](Self::sendto)
/// - use [`connect`](Self::connect) to associate a default peer and then use
///   [`send`](Self::send) / [`recv`](Self::recv)
///
/// Like the rest of lio's I/O surface, operations take ownership of buffers and
/// return them on completion.
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
  ///
  /// The returned socket is ready for datagram receive and for explicit-address
  /// sends via [`sendto`](Self::sendto).
  pub fn bind(addr: SocketAddr) -> Io<UdpBindSocket> {
    Io::from_op(UdpBindSocket::new(addr))
  }

  /// Creates a UDP socket and connects it to the specified remote address.
  ///
  /// A connected UDP socket keeps datagram semantics, but gains a default peer
  /// so callers can use [`send`](Self::send) and [`recv`](Self::recv) without
  /// specifying an address on each send.
  pub fn connect(addr: SocketAddr) -> Io<UdpConnectSocket> {
    Io::from_op(UdpConnectSocket::new(addr))
  }

  /// Receives one datagram into the provided buffer.
  ///
  /// This is most useful on a connected UDP socket where the peer is already
  /// fixed. If the sender address matters, use [`recvfrom`](Self::recvfrom).
  pub fn recv<V>(&self, vec: V) -> Io<Recv<V>>
  where
    V: buf::IoBufMutVec + std::marker::Send + Sync + 'static,
  {
    self.0.recv(vec)
  }

  /// Receives one datagram together with the sender's address.
  pub fn recvfrom<V>(&self, vec: V) -> Io<RecvFrom<V>>
  where
    V: buf::IoBufMutVec + std::marker::Send + Sync + 'static,
  {
    self.0.recvfrom(vec)
  }

  /// Sends one datagram from the provided buffer.
  ///
  /// This is most useful on a connected UDP socket with a default peer. For
  /// unconnected use, prefer [`sendto`](Self::sendto).
  pub fn send<V>(&self, vec: V) -> Io<Send<V>>
  where
    V: buf::IoBufVec + std::marker::Send + Sync + 'static,
  {
    self.0.send(vec)
  }

  /// Sends one datagram to a specific destination address.
  pub fn sendto<V>(&self, vec: V, addr: SocketAddr) -> Io<Send<V>>
  where
    V: buf::IoBufVec + std::marker::Send + Sync + 'static,
  {
    self.0.sendto(vec, addr)
  }

  /// Shuts down part or all of the socket.
  pub fn shutdown(&self, how: ShutdownHow) -> Io<Shutdown> {
    self.0.shutdown(how)
  }

  /// Returns the local address this socket is bound to.
  pub fn local_addr(&self) -> io::Result<SocketAddr> {
    self.0.local_addr()
  }

  /// Returns the default remote peer for a connected UDP socket.
  pub fn peer_addr(&self) -> io::Result<SocketAddr> {
    self.0.peer_addr()
  }
}
