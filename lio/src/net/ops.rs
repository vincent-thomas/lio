//! Internal operation types for network I/O.
//!
//! This module contains specialized operation types that adapt low-level I/O operations
//! to work with the high-level [`Socket`] type. These types implement
//! the [`TypedOp`] trait and are used internally by the networking API.
//!
//! Most users will not need to use these types directly, as they are returned by methods
//! on [`Socket`], [`TcpListener`], and [`TcpSocket`].
//!
//! # Available Operations
//!
//! - [`SocketAccept`]: Accept operation that returns a [`Socket`]
//! - [`SocketNew`]: Socket creation operation that returns a [`Socket`]

use std::{io, net::SocketAddr, os::fd::FromRawFd};

#[allow(unused_imports)] // TcpListener used in doc links
use crate::{
  api::{ops, resource::FromResource},
  net::{Socket, TcpListener, TcpSocket},
  typed_op::TypedOp,
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

impl TypedOp for SocketAccept {
  type Result = io::Result<(Socket, SocketAddr)>;

  fn into_op(&mut self) -> crate::op::Op {
    self.inner.into_op()
  }

  fn extract_result(self, res: isize) -> Self::Result {
    let (resource, addr) = self.inner.extract_result(res)?;
    Ok((Socket::from_resource(resource), addr))
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

impl TypedOp for SocketNew {
  type Result = io::Result<Socket>;

  fn into_op(&mut self) -> crate::op::Op {
    crate::op::Op::Socket {
      domain: self.domain,
      ty: self.ty,
      proto: self.proto,
    }
  }

  fn extract_result(self, res: isize) -> Self::Result {
    let result = if res < 0 {
      return Err(io::Error::from_raw_os_error(-res as i32));
    } else {
      res as std::os::fd::RawFd
    };
    // SAFETY: result is valid fd.
    let res = unsafe { crate::api::resource::Resource::from_raw_fd(result) };
    Ok(Socket::from_resource(res))
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

impl TypedOp for TcpAccept {
  type Result = io::Result<(TcpSocket, SocketAddr)>;

  fn into_op(&mut self) -> crate::op::Op {
    self.inner.into_op()
  }

  fn extract_result(self, res: isize) -> Self::Result {
    let (resource, addr) = self.inner.extract_result(res)?;
    Ok((TcpSocket::from_resource(resource), addr))
  }
}
