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

#[cfg(unix)]
use std::os::fd::FromRawFd;
use std::{io, net::SocketAddr};

#[allow(unused_imports)] // TcpListener used in doc links
use crate::{
  api::{
    op::{Action, Completion, OneshotOpModel, OpModel, OpResult},
    ops,
    resource::FromResource,
  },
  backend::op::{SockDomain, SockProto, SockType},
  net::{Socket, TcpListener, TcpSocket, UdpSocket},
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
  domain: i32,
  ty: i32,
  proto: i32,
}

impl SocketNew {
  pub(crate) fn new(domain: i32, ty: i32, proto: i32) -> Self {
    Self { domain, ty, proto }
  }
}

impl OpModel for SocketNew {
  type Item = io::Result<Socket>;

  fn action(&mut self) -> Action {
    Action::Io(crate::backend::op::Op::Socket {
      domain: SockDomain::from_raw(self.domain)
        .expect("SocketNew must be constructed with a valid socket domain"),
      ty: SockType::from_raw(self.ty)
        .expect("SocketNew must be constructed with a valid socket type"),
      proto: SockProto::from_raw(self.proto)
        .expect("SocketNew must be constructed with a valid socket protocol"),
    })
  }

  fn complete(&mut self, completion: Completion) -> OpResult<Self::Item> {
    if completion.result < 0 {
      return OpResult::Done(Err(io::Error::from_raw_os_error(
        (-completion.result) as i32,
      )));
    }

    #[cfg(unix)]
    {
      // SAFETY: successful socket creation returns a live file descriptor
      // owned by this operation.
      let resource = unsafe {
        crate::api::resource::Resource::from_raw_fd(completion.result as _)
      };
      OpResult::Done(Ok(Socket::from_resource(resource)))
    }

    #[cfg(windows)]
    {
      // SAFETY: successful socket creation returns a valid socket handle.
      let resource = unsafe {
        crate::api::resource::Resource::from_raw_handle(completion.result as _)
      };
      OpResult::Done(Ok(Socket::from_resource(resource)))
    }
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
  type Item = io::Result<(TcpSocket, SocketAddr)>;

  fn action(&mut self) -> Action {
    self.inner.action()
  }

  fn complete(&mut self, res: Completion) -> OpResult<Self::Item> {
    match self.inner.complete(res) {
      OpResult::Done(Ok((resource, addr))) => {
        OpResult::Done(Ok((TcpSocket::from_resource(resource), addr)))
      }
      OpResult::Done(Err(err)) => OpResult::Done(Err(err)),
      OpResult::Again => OpResult::Again,
      OpResult::Yield(item) => OpResult::Yield(
        item.map(|(resource, addr)| (TcpSocket::from_resource(resource), addr)),
      ),
    }
  }
}

pub struct TcpBindListener {
  state: TcpBindState,
  addr: SocketAddr,
}

enum TcpBindState {
  Socket(ops::Socket),
  Bind { resource: crate::api::resource::Resource },
  Listen { resource: crate::api::resource::Resource },
  Done,
}

impl TcpBindListener {
  pub(crate) fn new(addr: SocketAddr) -> Self {
    let domain =
      if addr.is_ipv4() { SockDomain::IPV4 } else { SockDomain::IPV6 };
    Self {
      state: TcpBindState::Socket(ops::Socket::new(
        domain,
        SockType::STREAM,
        SockProto::TCP,
      )),
      addr,
    }
  }
}

impl OpModel for TcpBindListener {
  type Item = io::Result<TcpListener>;

  fn action(&mut self) -> Action {
    match &mut self.state {
      TcpBindState::Socket(inner) => inner.action(),
      TcpBindState::Bind { resource } => {
        Action::Io(crate::backend::op::Op::Bind {
          fd: resource.clone(),
          addr: self.addr,
        })
      }
      TcpBindState::Listen { resource } => {
        Action::Io(crate::backend::op::Op::Listen {
          fd: resource.clone(),
          backlog: 128,
        })
      }
      TcpBindState::Done => panic!("TcpBindListener polled after completion"),
    }
  }

  fn complete(&mut self, completion: Completion) -> OpResult<Self::Item> {
    match &mut self.state {
      TcpBindState::Socket(inner) => match inner.complete(completion) {
        OpResult::Done(Ok(resource)) => {
          self.state = TcpBindState::Bind { resource };
          OpResult::Again
        }
        OpResult::Done(Err(err)) => {
          self.state = TcpBindState::Done;
          OpResult::Done(Err(err))
        }
        OpResult::Again => {
          panic!("socket creation unexpectedly requested Again")
        }
        OpResult::Yield(_) => panic!("socket creation unexpectedly yielded"),
      },
      TcpBindState::Bind { resource } => {
        if completion.result < 0 {
          self.state = TcpBindState::Done;
          OpResult::Done(Err(io::Error::from_raw_os_error(
            (-completion.result) as i32,
          )))
        } else {
          let resource = resource.clone();
          self.state = TcpBindState::Listen { resource };
          OpResult::Again
        }
      }
      TcpBindState::Listen { resource } => {
        let resource = resource.clone();
        self.state = TcpBindState::Done;
        if completion.result < 0 {
          OpResult::Done(Err(io::Error::from_raw_os_error(
            (-completion.result) as i32,
          )))
        } else {
          OpResult::Done(Ok(TcpListener::from_resource(resource)))
        }
      }
      TcpBindState::Done => {
        panic!("TcpBindListener received completion after finish")
      }
    }
  }
}

impl OneshotOpModel for TcpBindListener {}

pub struct TcpConnectSocket {
  state: TcpConnectState,
  addr: SocketAddr,
}

enum TcpConnectState {
  Socket(ops::Socket),
  Connect { resource: crate::api::resource::Resource },
  Done,
}

impl TcpConnectSocket {
  pub(crate) fn new(addr: SocketAddr) -> Self {
    let domain =
      if addr.is_ipv4() { SockDomain::IPV4 } else { SockDomain::IPV6 };
    Self {
      state: TcpConnectState::Socket(ops::Socket::new(
        domain,
        SockType::STREAM,
        SockProto::TCP,
      )),
      addr,
    }
  }
}

impl OpModel for TcpConnectSocket {
  type Item = io::Result<TcpSocket>;

  fn action(&mut self) -> Action {
    match &mut self.state {
      TcpConnectState::Socket(inner) => inner.action(),
      TcpConnectState::Connect { resource } => {
        Action::Io(crate::backend::op::Op::Connect {
          fd: resource.clone(),
          addr: crate::backend::op::socket_addr_into_buf(self.addr),
        })
      }
      TcpConnectState::Done => {
        panic!("TcpConnectSocket polled after completion")
      }
    }
  }

  fn complete(&mut self, completion: Completion) -> OpResult<Self::Item> {
    match &mut self.state {
      TcpConnectState::Socket(inner) => match inner.complete(completion) {
        OpResult::Done(Ok(resource)) => {
          self.state = TcpConnectState::Connect { resource };
          OpResult::Again
        }
        OpResult::Done(Err(err)) => {
          self.state = TcpConnectState::Done;
          OpResult::Done(Err(err))
        }
        OpResult::Again => {
          panic!("socket creation unexpectedly requested Again")
        }
        OpResult::Yield(_) => panic!("socket creation unexpectedly yielded"),
      },
      TcpConnectState::Connect { resource } => {
        let resource = resource.clone();
        self.state = TcpConnectState::Done;
        if completion.result < 0 {
          OpResult::Done(Err(io::Error::from_raw_os_error(
            (-completion.result) as i32,
          )))
        } else {
          OpResult::Done(Ok(TcpSocket::from_resource(resource)))
        }
      }
      TcpConnectState::Done => {
        panic!("TcpConnectSocket received completion after finish")
      }
    }
  }
}

impl OneshotOpModel for TcpConnectSocket {}

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
          addr: self.addr,
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
