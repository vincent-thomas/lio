//! Internal operation types for network I/O.
//!
//! This module contains specialized operation types that adapt low-level I/O operations
//! to work with the high-level [`Socket`] type. These types implement
//! [`OpModel`] and are used internally by the networking API.
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

use std::{io, marker::PhantomData, net::SocketAddr};

#[allow(unused_imports)] // TcpListener used in doc links
use crate::{
  api::{
    op::{Action, Completion, OneshotOpModel, OpModel, OpResult},
    ops,
    resource::FromResource,
  },
  backend::op::{
    SockDomain, SockProto, SockType, SocketAddrBuf, socket_addr_into_buf,
  },
  net::{Socket, TcpListener, TcpStream, UdpSocket, UnixListener, UnixStream},
};

/// Accept operation specialized for [`Socket`].
///
/// This type wraps the low-level [`Accept`](crate::api::ops::Accept) operation and adapts
/// its result to return a [`Socket`] instead of a raw [`Resource`](crate::api::resource::Resource).
///
/// You typically won't create this directly; it's returned by [`Socket::accept()`](crate::net::Socket::accept).
pub struct SocketAccept {
  inner: ops::Accept,
}

impl SocketAccept {
  pub(crate) fn new(res: crate::api::resource::Resource) -> Self {
    Self { inner: ops::Accept::new(res) }
  }
}

impl OpModel for SocketAccept {
  type Item = io::Result<(Socket, SocketAddr)>;

  fn action(&mut self) -> Action {
    self.inner.action()
  }

  fn complete(&mut self, res: Completion) -> OpResult<Self::Item> {
    match self.inner.complete(res) {
      OpResult::Done(Ok((resource, addr))) => {
        OpResult::Done(Ok((Socket::from_resource(resource), addr)))
      }
      OpResult::Done(Err(err)) => OpResult::Done(Err(err)),
      OpResult::Again => OpResult::Again,
      OpResult::Yield(item) => OpResult::Yield(
        item.map(|(resource, addr)| (Socket::from_resource(resource), addr)),
      ),
    }
  }
}

/// Socket creation operation specialized for [`Socket`].
///
/// This type wraps the low-level socket creation operation and adapts its result to
/// return a [`Socket`] instead of a raw [`Resource`](crate::api::resource::Resource).
///
/// You typically won't create this directly; it's returned by [`Socket::new()`](crate::net::Socket::new).
pub struct SocketNew {
  domain: SockDomain,
  ty: SockType,
  proto: SockProto,
}

impl SocketNew {
  pub(crate) fn new(
    domain: SockDomain,
    ty: SockType,
    proto: SockProto,
  ) -> Self {
    Self { domain, ty, proto }
  }
}

impl OpModel for SocketNew {
  type Item = io::Result<Socket>;

  fn action(&mut self) -> Action {
    Action::Io(crate::backend::op::Op::Socket {
      domain: self.domain,
      ty: self.ty,
      proto: self.proto,
    })
  }

  fn complete(&mut self, completion: Completion) -> OpResult<Self::Item> {
    if completion.result < 0 {
      return OpResult::Done(Err(io::Error::from_raw_os_error(
        (-completion.result) as i32,
      )));
    }

    // SAFETY: a successful socket completion carries a new owned socket.
    let resource = unsafe {
      crate::api::resource::Resource::from_completion(completion.result)
    };
    OpResult::Done(Ok(Socket::from_resource(resource)))
  }
}

pub struct TcpAccept {
  inner: ops::Accept,
}

impl TcpAccept {
  pub(crate) fn new(res: crate::api::resource::Resource) -> Self {
    Self { inner: ops::Accept::new(res) }
  }
}

// LEGACY `OpModel` impl parked during the serial-contract migration.
//
impl OpModel for TcpAccept {
  type Item = io::Result<(TcpStream, SocketAddr)>;

  fn action(&mut self) -> Action {
    self.inner.action()
  }

  fn complete(&mut self, res: Completion) -> OpResult<Self::Item> {
    match self.inner.complete(res) {
      OpResult::Done(Ok((resource, addr))) => {
        OpResult::Done(Ok((TcpStream::from_resource(resource), addr)))
      }
      OpResult::Done(Err(err)) => OpResult::Done(Err(err)),
      OpResult::Again => OpResult::Again,
      OpResult::Yield(item) => OpResult::Yield(
        item.map(|(resource, addr)| (TcpStream::from_resource(resource), addr)),
      ),
    }
  }
}

/// Socket-create/bind/listen operation that returns a listener of type `T`.
pub struct BindListener<T> {
  state: BindListenerState,
  addr: SocketAddrBuf,
  _listener: PhantomData<fn() -> T>,
}

/// Socket-create/bind/listen operation that returns a [`TcpListener`].
pub type TcpBindListener = BindListener<TcpListener>;

enum BindListenerState {
  Socket(ops::Socket),
  Bind { resource: crate::api::resource::Resource },
  Listen { resource: crate::api::resource::Resource },
  Done,
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
    let domain =
      if addr.is_ipv4() { SockDomain::IPV4 } else { SockDomain::IPV6 };
    Self::with_addr(domain, SockProto::TCP, socket_addr_into_buf(addr))
  }
}

impl BindListener<UnixListener> {
  pub(crate) fn unix(addr: SocketAddrBuf) -> Self {
    Self::with_addr(SockDomain::UNIX, SockProto::DEFAULT, addr)
  }
}

impl<T: FromResource + std::marker::Send + 'static> OpModel
  for BindListener<T>
{
  type Item = io::Result<T>;

  fn action(&mut self) -> Action {
    match &mut self.state {
      BindListenerState::Socket(inner) => inner.action(),
      BindListenerState::Bind { resource } => {
        Action::Io(crate::backend::op::Op::Bind {
          fd: resource.clone(),
          addr: self.addr,
        })
      }
      BindListenerState::Listen { resource } => {
        Action::Io(crate::backend::op::Op::Listen {
          fd: resource.clone(),
          backlog: 128,
        })
      }
      BindListenerState::Done => panic!("BindListener polled after completion"),
    }
  }

  fn complete(&mut self, completion: Completion) -> OpResult<Self::Item> {
    match &mut self.state {
      BindListenerState::Socket(inner) => match inner.complete(completion) {
        OpResult::Done(Ok(resource)) => {
          self.state = BindListenerState::Bind { resource };
          OpResult::Again
        }
        OpResult::Done(Err(err)) => {
          self.state = BindListenerState::Done;
          OpResult::Done(Err(err))
        }
        OpResult::Again => {
          panic!("socket creation unexpectedly requested Again")
        }
        OpResult::Yield(_) => panic!("socket creation unexpectedly yielded"),
      },
      BindListenerState::Bind { resource } => {
        if completion.result < 0 {
          self.state = BindListenerState::Done;
          OpResult::Done(Err(io::Error::from_raw_os_error(
            (-completion.result) as i32,
          )))
        } else {
          let resource = resource.clone();
          self.state = BindListenerState::Listen { resource };
          OpResult::Again
        }
      }
      BindListenerState::Listen { resource } => {
        let resource = resource.clone();
        self.state = BindListenerState::Done;
        if completion.result < 0 {
          OpResult::Done(Err(io::Error::from_raw_os_error(
            (-completion.result) as i32,
          )))
        } else {
          OpResult::Done(Ok(T::from_resource(resource)))
        }
      }
      BindListenerState::Done => {
        panic!("BindListener received completion after finish")
      }
    }
  }
}

impl<T: FromResource + std::marker::Send + 'static> OneshotOpModel
  for BindListener<T>
{
}

/// Socket-create/connect operation that returns a stream of type `T`.
pub struct StreamConnect<T> {
  state: StreamConnectState,
  addr: SocketAddrBuf,
  _stream: PhantomData<fn() -> T>,
}

/// Socket-create/connect operation that returns a [`TcpStream`].
pub type TcpStreamConnect = StreamConnect<TcpStream>;

enum StreamConnectState {
  Socket(ops::Socket),
  Connect { resource: crate::api::resource::Resource },
  Done,
}

impl<T> StreamConnect<T> {
  fn with_addr(
    domain: SockDomain,
    proto: SockProto,
    addr: SocketAddrBuf,
  ) -> Self {
    Self {
      state: StreamConnectState::Socket(ops::Socket::new(
        domain,
        SockType::STREAM,
        proto,
      )),
      addr,
      _stream: PhantomData,
    }
  }
}

impl TcpStreamConnect {
  pub(crate) fn new(addr: SocketAddr) -> Self {
    let domain =
      if addr.is_ipv4() { SockDomain::IPV4 } else { SockDomain::IPV6 };
    Self::with_addr(domain, SockProto::TCP, socket_addr_into_buf(addr))
  }
}

impl StreamConnect<UnixStream> {
  pub(crate) fn unix(addr: SocketAddrBuf) -> Self {
    Self::with_addr(SockDomain::UNIX, SockProto::DEFAULT, addr)
  }
}

impl<T: FromResource + std::marker::Send + 'static> OpModel
  for StreamConnect<T>
{
  type Item = io::Result<T>;

  fn action(&mut self) -> Action {
    match &mut self.state {
      StreamConnectState::Socket(inner) => inner.action(),
      StreamConnectState::Connect { resource } => {
        Action::Io(crate::backend::op::Op::Connect {
          fd: resource.clone(),
          addr: self.addr,
        })
      }
      StreamConnectState::Done => {
        panic!("StreamConnect polled after completion")
      }
    }
  }

  fn complete(&mut self, completion: Completion) -> OpResult<Self::Item> {
    match &mut self.state {
      StreamConnectState::Socket(inner) => match inner.complete(completion) {
        OpResult::Done(Ok(resource)) => {
          self.state = StreamConnectState::Connect { resource };
          OpResult::Again
        }
        OpResult::Done(Err(err)) => {
          self.state = StreamConnectState::Done;
          OpResult::Done(Err(err))
        }
        OpResult::Again => {
          panic!("socket creation unexpectedly requested Again")
        }
        OpResult::Yield(_) => panic!("socket creation unexpectedly yielded"),
      },
      StreamConnectState::Connect { resource } => {
        let resource = resource.clone();
        self.state = StreamConnectState::Done;
        if completion.result < 0 {
          OpResult::Done(Err(io::Error::from_raw_os_error(
            (-completion.result) as i32,
          )))
        } else {
          OpResult::Done(Ok(T::from_resource(resource)))
        }
      }
      StreamConnectState::Done => {
        panic!("StreamConnect received completion after finish")
      }
    }
  }
}

impl<T: FromResource + std::marker::Send + 'static> OneshotOpModel
  for StreamConnect<T>
{
}

/// Accept operation specialized for [`UnixStream`].
///
/// Unix peers are usually unnamed, so the peer address is not decoded.
pub struct UnixAccept {
  inner: ops::Accept,
}

impl UnixAccept {
  pub(crate) fn new(res: crate::api::resource::Resource) -> Self {
    Self { inner: ops::Accept::new(res) }
  }
}

impl OpModel for UnixAccept {
  type Item = io::Result<UnixStream>;

  fn action(&mut self) -> Action {
    self.inner.action()
  }

  fn complete(&mut self, completion: Completion) -> OpResult<Self::Item> {
    OpResult::Done(
      self.inner.complete_resource(completion).map(UnixStream::from_resource),
    )
  }
}

impl OneshotOpModel for UnixAccept {}

pub struct UdpBindSocket {
  state: UdpBindState,
  addr: SocketAddr,
}

enum UdpBindState {
  Socket(ops::Socket),
  Bind { resource: crate::api::resource::Resource },
  Done,
}

impl UdpBindSocket {
  pub(crate) fn new(addr: SocketAddr) -> Self {
    let domain =
      if addr.is_ipv4() { SockDomain::IPV4 } else { SockDomain::IPV6 };
    Self {
      state: UdpBindState::Socket(ops::Socket::new(
        domain,
        SockType::DGRAM,
        SockProto::DEFAULT,
      )),
      addr,
    }
  }
}

impl OpModel for UdpBindSocket {
  type Item = io::Result<UdpSocket>;

  fn action(&mut self) -> Action {
    match &mut self.state {
      UdpBindState::Socket(inner) => inner.action(),
      UdpBindState::Bind { resource } => {
        Action::Io(crate::backend::op::Op::Bind {
          fd: resource.clone(),
          addr: crate::backend::op::socket_addr_into_buf(self.addr),
        })
      }
      UdpBindState::Done => panic!("UdpBindSocket polled after completion"),
    }
  }

  fn complete(&mut self, completion: Completion) -> OpResult<Self::Item> {
    match &mut self.state {
      UdpBindState::Socket(inner) => match inner.complete(completion) {
        OpResult::Done(Ok(resource)) => {
          self.state = UdpBindState::Bind { resource };
          OpResult::Again
        }
        OpResult::Done(Err(err)) => {
          self.state = UdpBindState::Done;
          OpResult::Done(Err(err))
        }
        OpResult::Again => {
          panic!("socket creation unexpectedly requested Again")
        }
        OpResult::Yield(_) => panic!("socket creation unexpectedly yielded"),
      },
      UdpBindState::Bind { resource } => {
        let resource = resource.clone();
        self.state = UdpBindState::Done;
        if completion.result < 0 {
          OpResult::Done(Err(io::Error::from_raw_os_error(
            (-completion.result) as i32,
          )))
        } else {
          OpResult::Done(Ok(UdpSocket::from_resource(resource)))
        }
      }
      UdpBindState::Done => {
        panic!("UdpBindSocket received completion after finish")
      }
    }
  }
}

impl OneshotOpModel for UdpBindSocket {}

pub struct UdpConnectSocket {
  state: UdpConnectState,
  addr: SocketAddr,
}

enum UdpConnectState {
  Socket(ops::Socket),
  Connect { resource: crate::api::resource::Resource },
  Done,
}

impl UdpConnectSocket {
  pub(crate) fn new(addr: SocketAddr) -> Self {
    let domain =
      if addr.is_ipv4() { SockDomain::IPV4 } else { SockDomain::IPV6 };
    Self {
      state: UdpConnectState::Socket(ops::Socket::new(
        domain,
        SockType::DGRAM,
        SockProto::DEFAULT,
      )),
      addr,
    }
  }
}

impl OpModel for UdpConnectSocket {
  type Item = io::Result<UdpSocket>;

  fn action(&mut self) -> Action {
    match &mut self.state {
      UdpConnectState::Socket(inner) => inner.action(),
      UdpConnectState::Connect { resource } => {
        Action::Io(crate::backend::op::Op::Connect {
          fd: resource.clone(),
          addr: crate::backend::op::socket_addr_into_buf(self.addr),
        })
      }
      UdpConnectState::Done => {
        panic!("UdpConnectSocket polled after completion")
      }
    }
  }

  fn complete(&mut self, completion: Completion) -> OpResult<Self::Item> {
    match &mut self.state {
      UdpConnectState::Socket(inner) => match inner.complete(completion) {
        OpResult::Done(Ok(resource)) => {
          self.state = UdpConnectState::Connect { resource };
          OpResult::Again
        }
        OpResult::Done(Err(err)) => {
          self.state = UdpConnectState::Done;
          OpResult::Done(Err(err))
        }
        OpResult::Again => {
          panic!("socket creation unexpectedly requested Again")
        }
        OpResult::Yield(_) => panic!("socket creation unexpectedly yielded"),
      },
      UdpConnectState::Connect { resource } => {
        let resource = resource.clone();
        self.state = UdpConnectState::Done;
        if completion.result < 0 {
          OpResult::Done(Err(io::Error::from_raw_os_error(
            (-completion.result) as i32,
          )))
        } else {
          OpResult::Done(Ok(UdpSocket::from_resource(resource)))
        }
      }
      UdpConnectState::Done => {
        panic!("UdpConnectSocket received completion after finish")
      }
    }
  }
}

impl OneshotOpModel for UdpConnectSocket {}
