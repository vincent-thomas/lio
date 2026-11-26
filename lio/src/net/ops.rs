//! Internal operation types for network I/O.
//!
//! This module contains specialized operation types that adapt low-level I/O operations
//! to work with the high-level [`Socket`] type. These types are
//! [models](crate::model) and are used internally by the networking API.
//!
//! Most users will not need to use these types directly, as they are returned by methods
//! on [`Socket`], [`TcpListener`], and [`TcpSocket`].
//!
//! # Available Operations
//!
//! - [`SocketAccept`]: Accept operation that returns a [`Socket`]
//! - [`SocketNew`]: Socket creation operation that returns a [`Socket`]
//! - [`TcpBindListener`]: Socket-create/bind/listen operation that returns a [`TcpListener`]
//! - [`TcpConnectSocket`]: Socket-create/connect operation that returns a [`TcpSocket`]
//! - [`UdpBindSocket`]: Socket-create/bind operation that returns a [`UdpSocket`]
//! - [`UdpConnectSocket`]: Socket-create/connect operation that returns a [`UdpSocket`]
//! - [`BindListener`] / [`StreamConnect`]: generic create/bind/listen and
//!   create/connect operations, also used for Unix domain sockets
//! - [`UnixAccept`]: Accept operation that returns a [`UnixStream`]

use crate::ready;
use std::{io, marker::PhantomData, net::SocketAddr};

#[allow(unused_imports)] // TcpListener used in doc links
use crate::{
  api::{
    ops,
    resource::{FromResource, Resource},
  },
  backend::op::{
    SockDomain, SockProto, SockType, SocketAddrBuf, socket_addr_into_buf,
  },
  model::{Cx, OpModel, Step},
  net::{Socket, TcpListener, TcpStream, UdpSocket, UnixListener, UnixStream},
};

fn ip_domain(addr: &SocketAddr) -> SockDomain {
  if addr.is_ipv4() { SockDomain::IPV4 } else { SockDomain::IPV6 }
}

/// Accept operation specialized for [`Socket`].
///
/// This type wraps the low-level [`Accept`](crate::api::ops::Accept) operation and adapts
/// its result to return a [`Socket`] instead of a raw [`Resource`](crate::api::resource::Resource).
///
/// You typically won't create this directly; it's returned by [`Socket::accept()`](crate::net::Socket::accept).
#[must_use = "an operation does nothing until it is run or polled"]
pub struct SocketAccept {
  inner: ops::Accept,
}

impl SocketAccept {
  pub(crate) fn new(res: Resource) -> Self {
    Self { inner: ops::Accept::new(res) }
  }
}

runnable!([] SocketAccept);

impl OpModel for SocketAccept {
  type Item = io::Result<(Socket, SocketAddr)>;

  fn poll(&mut self, cx: &mut Cx<'_>) -> Step<Self::Item> {
    Step::Done(
      ready!(self.inner.poll(cx))
        .map(|(resource, addr)| (Socket::from_resource(resource), addr)),
    )
  }
}

/// Socket creation operation specialized for [`Socket`].
///
/// This type wraps the low-level socket creation operation and adapts its result to
/// return a [`Socket`] instead of a raw [`Resource`](crate::api::resource::Resource).
///
/// You typically won't create this directly; it's returned by [`Socket::new()`](crate::net::Socket::new).
#[must_use = "an operation does nothing until it is run or polled"]
pub struct SocketNew {
  inner: ops::Socket,
}

impl SocketNew {
  pub(crate) fn new(
    domain: SockDomain,
    ty: SockType,
    proto: SockProto,
  ) -> Self {
    Self { inner: ops::Socket::new(domain, ty, proto) }
  }
}

runnable!([] SocketNew);

impl OpModel for SocketNew {
  type Item = io::Result<Socket>;

  fn poll(&mut self, cx: &mut Cx<'_>) -> Step<Self::Item> {
    Step::Done(ready!(self.inner.poll(cx)).map(Socket::from_resource))
  }
}

#[must_use = "an operation does nothing until it is run or polled"]
pub struct TcpAccept {
  inner: ops::Accept,
}

impl TcpAccept {
  pub(crate) fn new(res: Resource) -> Self {
    Self { inner: ops::Accept::new(res) }
  }
}

runnable!([] TcpAccept);

impl OpModel for TcpAccept {
  type Item = io::Result<(TcpStream, SocketAddr)>;

  fn poll(&mut self, cx: &mut Cx<'_>) -> Step<Self::Item> {
    Step::Done(
      ready!(self.inner.poll(cx))
        .map(|(resource, addr)| (TcpStream::from_resource(resource), addr)),
    )
  }
}

/// Socket-create/bind/listen operation that returns a listener of type `T`.
#[must_use = "an operation does nothing until it is run or polled"]
pub struct BindListener<T> {
  state: BindListenerState,
  addr: SocketAddrBuf,
  _listener: PhantomData<fn() -> T>,
}

/// Socket-create/bind/listen operation that returns a [`TcpListener`].
pub type TcpBindListener = BindListener<TcpListener>;

enum BindListenerState {
  Socket(ops::Socket),
  Bind { resource: Resource, bind: ops::Bind },
  Listen { resource: Resource, listen: ops::Listen },
}

impl<T> BindListener<T> {
  fn with_addr(
    domain: SockDomain,
    proto: SockProto,
    addr: SocketAddrBuf,
  ) -> Self {
    Self {
      state: BindListenerState::Socket(ops::Socket::new(
        domain,
        SockType::STREAM,
        proto,
      )),
      addr,
      _listener: PhantomData,
    }
  }
}

impl TcpBindListener {
  pub(crate) fn new(addr: SocketAddr) -> Self {
    Self::with_addr(
      ip_domain(&addr),
      SockProto::TCP,
      socket_addr_into_buf(addr),
    )
  }
}

impl BindListener<UnixListener> {
  pub(crate) fn unix(addr: SocketAddrBuf) -> Self {
    Self::with_addr(SockDomain::UNIX, SockProto::DEFAULT, addr)
  }
}

runnable!([T: FromResource + 'static] BindListener<T>);

impl<T: FromResource + 'static> OpModel for BindListener<T> {
  type Item = io::Result<T>;

  fn poll(&mut self, cx: &mut Cx<'_>) -> Step<Self::Item> {
    loop {
      self.state = match &mut self.state {
        BindListenerState::Socket(socket) => match ready!(socket.poll(cx)) {
          Ok(resource) => BindListenerState::Bind {
            bind: ops::Bind::new(resource.clone(), self.addr),
            resource,
          },
          Err(err) => return Step::Done(Err(err)),
        },
        BindListenerState::Bind { resource, bind } => {
          match ready!(bind.poll(cx)) {
            Ok(()) => BindListenerState::Listen {
              listen: ops::Listen::new(resource.clone(), 128),
              resource: resource.clone(),
            },
            Err(err) => return Step::Done(Err(err)),
          }
        }
        BindListenerState::Listen { resource, listen } => {
          let listening = ready!(listen.poll(cx));
          return Step::Done(
            listening.map(|()| T::from_resource(resource.clone())),
          );
        }
      };
    }
  }
}

/// A socket created and then connected to an address.
///
/// The step [`StreamConnect`] and [`UdpConnectSocket`] share.
struct Connecting {
  state: ConnectingState,
  addr: SocketAddrBuf,
}

enum ConnectingState {
  Socket(ops::Socket),
  Connect { resource: Resource, connect: ops::Connect },
}

impl Connecting {
  fn new(
    domain: SockDomain,
    ty: SockType,
    proto: SockProto,
    addr: SocketAddrBuf,
  ) -> Self {
    Self {
      state: ConnectingState::Socket(ops::Socket::new(domain, ty, proto)),
      addr,
    }
  }
}

impl OpModel for Connecting {
  type Item = io::Result<Resource>;

  fn poll(&mut self, cx: &mut Cx<'_>) -> Step<Self::Item> {
    loop {
      match &mut self.state {
        ConnectingState::Socket(socket) => match ready!(socket.poll(cx)) {
          Ok(resource) => {
            self.state = ConnectingState::Connect {
              connect: ops::Connect::new(resource.clone(), self.addr),
              resource,
            };
          }
          Err(err) => return Step::Done(Err(err)),
        },
        ConnectingState::Connect { resource, connect } => {
          let connected = ready!(connect.poll(cx));
          return Step::Done(connected.map(|()| resource.clone()));
        }
      }
    }
  }
}

/// Socket-create/connect operation that returns a stream of type `T`.
#[must_use = "an operation does nothing until it is run or polled"]
pub struct StreamConnect<T> {
  inner: Connecting,
  _stream: PhantomData<fn() -> T>,
}

/// Socket-create/connect operation that returns a [`TcpStream`].
pub type TcpStreamConnect = StreamConnect<TcpStream>;

impl TcpStreamConnect {
  pub(crate) fn new(addr: SocketAddr) -> Self {
    Self {
      inner: Connecting::new(
        ip_domain(&addr),
        SockType::STREAM,
        SockProto::TCP,
        socket_addr_into_buf(addr),
      ),
      _stream: PhantomData,
    }
  }
}

impl StreamConnect<UnixStream> {
  pub(crate) fn unix(addr: SocketAddrBuf) -> Self {
    Self {
      inner: Connecting::new(
        SockDomain::UNIX,
        SockType::STREAM,
        SockProto::DEFAULT,
        addr,
      ),
      _stream: PhantomData,
    }
  }
}

runnable!([T: FromResource + 'static] StreamConnect<T>);

impl<T: FromResource + 'static> OpModel for StreamConnect<T> {
  type Item = io::Result<T>;

  fn poll(&mut self, cx: &mut Cx<'_>) -> Step<Self::Item> {
    Step::Done(ready!(self.inner.poll(cx)).map(T::from_resource))
  }
}

/// Accept operation specialized for [`UnixStream`].
///
/// Unix peers are usually unnamed, so the peer address is not reported.
#[must_use = "an operation does nothing until it is run or polled"]
pub struct UnixAccept {
  inner: ops::AcceptUnix,
}

impl UnixAccept {
  pub(crate) fn new(res: Resource) -> Self {
    Self { inner: ops::AcceptUnix::new(res) }
  }
}

runnable!([] UnixAccept);

impl OpModel for UnixAccept {
  type Item = io::Result<UnixStream>;

  fn poll(&mut self, cx: &mut Cx<'_>) -> Step<Self::Item> {
    Step::Done(
      ready!(self.inner.poll(cx))
        .map(|(resource, _peer)| UnixStream::from_resource(resource)),
    )
  }
}

#[must_use = "an operation does nothing until it is run or polled"]
pub struct UdpBindSocket {
  state: UdpBindState,
  addr: SocketAddr,
}

enum UdpBindState {
  Socket(ops::Socket),
  Bind { resource: Resource, bind: ops::Bind },
}

impl UdpBindSocket {
  pub(crate) fn new(addr: SocketAddr) -> Self {
    Self {
      state: UdpBindState::Socket(ops::Socket::new(
        ip_domain(&addr),
        SockType::DGRAM,
        SockProto::DEFAULT,
      )),
      addr,
    }
  }
}

runnable!([] UdpBindSocket);

impl OpModel for UdpBindSocket {
  type Item = io::Result<UdpSocket>;

  fn poll(&mut self, cx: &mut Cx<'_>) -> Step<Self::Item> {
    loop {
      match &mut self.state {
        UdpBindState::Socket(socket) => match ready!(socket.poll(cx)) {
          Ok(resource) => {
            self.state = UdpBindState::Bind {
              bind: ops::Bind::new(
                resource.clone(),
                crate::backend::op::socket_addr_into_buf(self.addr),
              ),
              resource,
            };
          }
          Err(err) => return Step::Done(Err(err)),
        },
        UdpBindState::Bind { resource, bind } => {
          let bound = ready!(bind.poll(cx));
          return Step::Done(
            bound.map(|()| UdpSocket::from_resource(resource.clone())),
          );
        }
      }
    }
  }
}

#[must_use = "an operation does nothing until it is run or polled"]
pub struct UdpConnectSocket {
  inner: Connecting,
}

impl UdpConnectSocket {
  pub(crate) fn new(addr: SocketAddr) -> Self {
    Self {
      inner: Connecting::new(
        ip_domain(&addr),
        SockType::DGRAM,
        SockProto::DEFAULT,
        socket_addr_into_buf(addr),
      ),
    }
  }
}

runnable!([] UdpConnectSocket);

impl OpModel for UdpConnectSocket {
  type Item = io::Result<UdpSocket>;

  fn poll(&mut self, cx: &mut Cx<'_>) -> Step<Self::Item> {
    Step::Done(ready!(self.inner.poll(cx)).map(UdpSocket::from_resource))
  }
}
