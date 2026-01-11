//! Internal operation types for network I/O.
//!
//! This module contains specialized operation types that adapt low-level I/O operations
//! to work with the high-level [`Socket`](crate::net::Socket) type. These types implement
//! the [`Operation`] trait and are used internally by the networking API.
//!
//! Most users will not need to use these types directly, as they are returned by methods
//! on [`Socket`](crate::net::Socket), [`TcpListener`](crate::net::TcpListener), and
//! [`TcpSocket`](crate::net::TcpSocket).
//!
//! # Available Operations
//!
//! - [`SocketAccept`]: Accept operation that returns a [`Socket`](crate::net::Socket)
//! - [`SocketNew`]: Socket creation operation that returns a [`Socket`](crate::net::Socket)

use std::{io, net::SocketAddr};

use crate::{
  api::{ops, resource::FromResource},
  net::{Socket, TcpSocket},
  operation::{OpMeta, Operation, OperationExt},
};

/// Accept operation specialized for [`Socket`].
///
/// This type wraps the low-level [`Accept`](crate::operation::ops::Accept) operation and adapts
/// its result to return a [`Socket`] instead of a raw [`Resource`](crate::api::resource::Resource).
///
/// You typically won't create this directly; it's returned by [`Socket::accept()`](crate::net::Socket::accept).
pub struct SocketAccept(pub(crate) ops::Accept);

impl OperationExt for SocketAccept {
  type Result = io::Result<(Socket, SocketAddr)>;
}

impl Operation for SocketAccept {
  fn run_blocking(&self) -> isize {
    self.0.run_blocking()
  }
  fn meta(&self) -> OpMeta {
    self.0.meta()
  }
  fn result(&mut self, ret: isize) -> *const () {
    let ptr =
      self.0.result(ret) as *const <ops::Accept as OperationExt>::Result;

    // SAFETY: The pointer was just created by Accept::result and points to valid Accept::Result
    let accept_result = unsafe { ptr.read() };

    let socket_result = accept_result
      .map(|(resource, addr)| (Socket::from_resource(resource), addr));

    // Box the result and return pointer
    Box::into_raw(Box::new(socket_result)) as *const ()
  }
  fn cap(&self) -> i32 {
    self.0.cap()
  }

  #[cfg(linux)]
  fn create_entry(&self) -> io_uring::squeue::Entry {
    self.0.create_entry()
  }
}

/// Socket creation operation specialized for [`Socket`].
///
/// This type wraps the low-level socket creation operation and adapts its result to
/// return a [`Socket`] instead of a raw [`Resource`](crate::api::resource::Resource).
///
/// You typically won't create this directly; it's returned by [`Socket::new()`](crate::net::Socket::new).
pub struct SocketNew(pub(crate) ops::Socket);

impl OperationExt for SocketNew {
  type Result = io::Result<Socket>;
}

impl Operation for SocketNew {
  fn run_blocking(&self) -> isize {
    self.0.run_blocking()
  }
  fn meta(&self) -> OpMeta {
    self.0.meta()
  }
  fn result(&mut self, ret: isize) -> *const () {
    let ptr =
      self.0.result(ret) as *const <ops::Socket as OperationExt>::Result;

    // SAFETY: The pointer was just created by Accept::result and points to valid Accept::Result
    let accept_result = unsafe { ptr.read() };

    let socket_result = accept_result.map(Socket::from_resource);

    // Box the result and return pointer
    Box::into_raw(Box::new(socket_result)) as *const ()
  }
  fn cap(&self) -> i32 {
    self.0.cap()
  }

  #[cfg(linux)]
  fn create_entry(&self) -> io_uring::squeue::Entry {
    self.0.create_entry()
  }
}

pub struct TcpAccept(pub(crate) ops::Accept);

impl OperationExt for TcpAccept {
  type Result = io::Result<(TcpSocket, SocketAddr)>;
}

impl Operation for TcpAccept {
  fn run_blocking(&self) -> isize {
    self.0.run_blocking()
  }
  fn meta(&self) -> OpMeta {
    self.0.meta()
  }
  fn result(&mut self, ret: isize) -> *const () {
    let ptr =
      self.0.result(ret) as *const <ops::Accept as OperationExt>::Result;

    // SAFETY: The pointer was just created by Accept::result and points to valid Accept::Result
    let accept_result = unsafe { ptr.read() };

    let socket_result = accept_result
      .map(|(resource, addr)| (TcpSocket::from_resource(resource), addr));

    // Box the result and return pointer
    Box::into_raw(Box::new(socket_result)) as *const ()
  }
  fn cap(&self) -> i32 {
    self.0.cap()
  }

  #[cfg(linux)]
  fn create_entry(&self) -> io_uring::squeue::Entry {
    self.0.create_entry()
  }
}
