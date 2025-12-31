//! Test utilities for integration tests.
//!
//! This module provides helper functions for creating sockets using lio's
//! async operations. Only available when building tests.

use crate::{op::Progress, resource::Resource, socket};

/// Creates a Unix stream socket using lio operations (blocking).
///
/// Returns the Resource which must be closed by the caller using `lio::close()`.
#[doc(hidden)]
pub fn unix_stream_socket() -> Resource {
  socket(libc::AF_UNIX, libc::SOCK_STREAM, 0).blocking().unwrap()
}

/// Creates a Unix datagram socket using lio operations (blocking).
///
/// Returns the Resource which must be closed by the caller using `lio::close()`.
#[doc(hidden)]
pub fn unix_dgram_socket() -> Resource {
  socket(libc::AF_UNIX, libc::SOCK_DGRAM, 0).blocking().unwrap()
}

/// Creates a TCP IPv4 socket using lio operations.
///
/// Returns a Progress that can be used with `.send()`, `.when_done()`, `.blocking()`, etc.
#[doc(hidden)]
pub fn tcp_socket() -> Progress<crate::op::Socket> {
  socket(libc::AF_INET, libc::SOCK_STREAM, libc::IPPROTO_TCP)
}

/// Creates a TCP IPv6 socket using lio operations.
///
/// Returns a Progress that can be used with `.send()`, `.when_done()`, `.blocking()`, etc.
#[doc(hidden)]
pub fn tcp6_socket() -> Progress<crate::op::Socket> {
  socket(libc::AF_INET6, libc::SOCK_STREAM, libc::IPPROTO_TCP)
}

/// Creates a UDP IPv4 socket using lio operations.
///
/// Returns a Progress that can be used with `.send()`, `.when_done()`, `.blocking()`, etc.
#[doc(hidden)]
pub fn udp_socket() -> Progress<crate::op::Socket> {
  socket(libc::AF_INET, libc::SOCK_DGRAM, libc::IPPROTO_UDP)
}

/// Creates a UDP IPv6 socket using lio operations.
///
/// Returns a Progress that can be used with `.send()`, `.when_done()`, `.blocking()`, etc.
#[doc(hidden)]
pub fn udp6_socket() -> Progress<crate::op::Socket> {
  socket(libc::AF_INET6, libc::SOCK_DGRAM, libc::IPPROTO_UDP)
}
