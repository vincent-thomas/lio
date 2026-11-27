//! Calls on `wasi:sockets/types@0.3.2`.
//!
//! As in [`fs`](crate::fs), each operation value holds a call's arguments
//! and result. TCP data moves over byte streams: pass the stream from
//! `TcpSocket::receive` to
//! [`WaitableSet::add_reader`](crate::WaitableSet::add_reader), and hand
//! `TcpSocket::send` the readable end of a stream whose writable end went
//! to [`WaitableSet::add_writer`](crate::WaitableSet::add_writer). A
//! listener is a stream of sockets: see
//! [`WaitableSet::add_listener`](crate::WaitableSet::add_listener).
//!
//! Calls that do not wait are not operations: `create`, `bind`, `listen`,
//! the UDP `connect` and `disconnect`, addresses and socket options are
//! methods of [`TcpSocket`] and [`UdpSocket`], and dropping a socket closes
//! it. Dropping the stream given
//! to `send` shuts down the write side; closing the `receive` stream shuts
//! down the read side.

use core::cell::UnsafeCell;
use core::marker::PhantomData;

pub use wasip3::sockets::types::{
  ErrorCode, IpAddressFamily, IpSocketAddress, Ipv4SocketAddress,
  Ipv6SocketAddress, TcpSocket, UdpSocket,
};
use wasip3::wit_bindgen::FutureReader;
use wasip3::wit_future::FuturePayload;

use crate::reply::{LiftError, NO_RESULT, ResultFuture, Slot, Words, call};
use crate::sealed::{self, FutureStart};
use crate::sys::wasm_imports;
use crate::{Call, ReadableFuture};

wasm_imports!("wasi:sockets/types@0.3.2" {
  "[async-lower][method]tcp-socket.connect"
    fn tcp_connect(params: *const u8, reply: *mut u8) -> i32;
  "[async-lower][method]udp-socket.send"
    fn udp_send(params: *const u8, reply: *mut u8) -> i32;
  "[async-lower][method]udp-socket.receive"
    fn udp_receive(socket: i32, reply: *mut u8) -> i32;
});

impl LiftError for ErrorCode {
  fn plain(case: u8) -> Option<Self> {
    Some(match case {
      0 => ErrorCode::AccessDenied,
      1 => ErrorCode::NotSupported,
      2 => ErrorCode::InvalidArgument,
      3 => ErrorCode::OutOfMemory,
      4 => ErrorCode::Timeout,
      5 => ErrorCode::InvalidState,
      6 => ErrorCode::AddressNotBindable,
      7 => ErrorCode::AddressInUse,
      8 => ErrorCode::RemoteUnreachable,
      9 => ErrorCode::ConnectionRefused,
      10 => ErrorCode::ConnectionBroken,
      11 => ErrorCode::ConnectionReset,
      12 => ErrorCode::ConnectionAborted,
      13 => ErrorCode::DatagramTooLarge,
      14 => return None,
      other => unreachable!("unknown error-code case {other}"),
    })
  }

  fn other(message: Option<String>) -> Self {
    ErrorCode::Other(message)
  }
}

/// `ip-socket-address` in the canonical ABI: a case byte, then at offset 4
/// either an IPv4 or an IPv6 socket address. Multi-byte fields are
/// little-endian.
#[derive(Clone, Copy)]
#[repr(C, align(4))]
struct AddressAbi([u8; 32]);

impl AddressAbi {
  const fn zeroed() -> Self {
    Self([0; 32])
  }

  fn put_u16(&mut self, at: usize, value: u16) {
    self.0[at..at + 2].copy_from_slice(&value.to_le_bytes());
  }

  fn put_u32(&mut self, at: usize, value: u32) {
    self.0[at..at + 4].copy_from_slice(&value.to_le_bytes());
  }

  fn u16_at(&self, at: usize) -> u16 {
    u16::from_le_bytes([self.0[at], self.0[at + 1]])
  }

  fn u32_at(&self, at: usize) -> u32 {
    u32::from_le_bytes([
      self.0[at],
      self.0[at + 1],
      self.0[at + 2],
      self.0[at + 3],
    ])
  }

  fn lower(address: IpSocketAddress) -> Self {
    let mut abi = Self::zeroed();
    match address {
      IpSocketAddress::Ipv4(v4) => {
        abi.0[0] = 0;
        abi.put_u16(4, v4.port);
        let (a, b, c, d) = v4.address;
        abi.0[6..10].copy_from_slice(&[a, b, c, d]);
      }
      IpSocketAddress::Ipv6(v6) => {
        abi.0[0] = 1;
        abi.put_u16(4, v6.port);
        abi.put_u32(8, v6.flow_info);
        let (a, b, c, d, e, f, g, h) = v6.address;
        for (index, group) in [a, b, c, d, e, f, g, h].into_iter().enumerate() {
          abi.put_u16(12 + 2 * index, group);
        }
        abi.put_u32(28, v6.scope_id);
      }
    }
    abi
  }

  fn lift(&self) -> IpSocketAddress {
    match self.0[0] {
      0 => IpSocketAddress::Ipv4(Ipv4SocketAddress {
        port: self.u16_at(4),
        address: (self.0[6], self.0[7], self.0[8], self.0[9]),
      }),
      1 => {
        let group = |index: usize| self.u16_at(12 + 2 * index);
        IpSocketAddress::Ipv6(Ipv6SocketAddress {
          port: self.u16_at(4),
          flow_info: self.u32_at(8),
          address: (
            group(0),
            group(1),
            group(2),
            group(3),
            group(4),
            group(5),
            group(6),
            group(7),
          ),
          scope_id: self.u32_at(28),
        })
      }
      other => unreachable!("unknown ip-socket-address case {other}"),
    }
  }
}

// ──────────────────────────────────── TCP ───────────────────────────────────

/// Arguments of `tcp-socket.connect`.
#[repr(C)]
struct ConnectArgs {
  socket: i32,
  remote: AddressAbi,
}

const _: () = {
  assert!(size_of::<ConnectArgs>() == 36);
  assert!(core::mem::offset_of!(ConnectArgs, remote) == 4);
};

/// Connects a TCP socket to a remote address, like `connect`.
pub struct Connect<'a> {
  args: ConnectArgs,
  reply: Slot<(), ErrorCode>,
  borrows: PhantomData<&'a TcpSocket>,
}

impl<'a> Connect<'a> {
  pub fn new(socket: &'a TcpSocket, remote: IpSocketAddress) -> Self {
    Self {
      args: ConnectArgs {
        socket: socket.handle().cast_signed(),
        remote: AddressAbi::lower(remote),
      },
      reply: Slot::new(),
      borrows: PhantomData,
    }
  }
}

call!(Connect => (), |op, reply| {
  imports::tcp_connect((&raw const op.args).cast(), reply)
});

/// The listening end of a TCP socket, owned by a
/// [`WaitableSet`](crate::WaitableSet).
///
/// Returned by
/// [`WaitableSet::add_listener`](crate::WaitableSet::add_listener). It stays
/// valid until
/// [`WaitableSet::close_listener`](crate::WaitableSet::close_listener) or
/// until the set is dropped.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Listener(pub(crate) u32);

/// No socket is stored. A real handle is never this value: `wit-bindgen`
/// reserves it for the same purpose.
const NO_SOCKET: u32 = u32::MAX;

/// Where an accepted connection lands.
///
/// Pass it to [`WaitableSet::accept`](crate::WaitableSet::accept). After
/// the accept ends as
/// [`StreamResult::Completed(1)`](crate::StreamResult::Completed), take the
/// connection with [`socket`](Self::socket).
pub struct Accepted {
  /// The host writes the new socket's handle through a pointer into this.
  handle: UnsafeCell<u32>,
}

impl Accepted {
  pub const fn new() -> Self {
    Self { handle: UnsafeCell::new(NO_SOCKET) }
  }

  /// Takes the accepted connection.
  ///
  /// Returns `None` if no connection arrived, the accept was cancelled, or
  /// the connection was already taken.
  pub fn socket(&mut self) -> Option<TcpSocket> {
    match core::mem::replace(self.handle.get_mut(), NO_SOCKET) {
      NO_SOCKET => None,
      // SAFETY: the host wrote this handle as an owned socket, and the slot
      // was cleared, so it is adopted once.
      handle => Some(unsafe { TcpSocket::from_handle(handle) }),
    }
  }

  /// Releases any connection still here and returns where the host writes
  /// the next one.
  pub(crate) fn arm(&mut self) -> *mut u8 {
    drop(self.socket());
    self.handle.get().cast()
  }
}

impl Default for Accepted {
  fn default() -> Self {
    Self::new()
  }
}

impl Drop for Accepted {
  fn drop(&mut self) {
    // An untaken connection is closed.
    drop(self.socket());
  }
}

/// How a TCP stream ended: `Ok` when the peer closed it cleanly, or the
/// error that cut it short.
///
/// `TcpSocket::send` and `TcpSocket::receive` return it as a future. Read
/// it with [`WaitableSet::read_future`](crate::WaitableSet::read_future) to
/// tell an orderly close from a reset. It is delivered once the stream has
/// ended.
///
/// # Panics
///
/// Reading it again after it was delivered panics: a status is delivered
/// once.
pub struct StreamStatus {
  future: ResultFuture<ErrorCode>,
}

impl StreamStatus {
  pub fn new(result: FutureReader<Result<(), ErrorCode>>) -> Self {
    let vtable = <Result<(), ErrorCode> as FuturePayload>::VTABLE;
    Self { future: ResultFuture::new(result, vtable) }
  }

  /// Takes the status.
  ///
  /// Returns `None` if it has not arrived, was cancelled, or was already
  /// taken.
  pub fn result(&mut self) -> Option<Result<(), ErrorCode>> {
    self.future.take()
  }
}

impl ReadableFuture for StreamStatus {}

impl sealed::Future for StreamStatus {
  unsafe fn start_read(&mut self) -> FutureStart {
    // SAFETY: the caller keeps `self` in place until the read completes.
    unsafe { self.future.start() }
  }
}

// ──────────────────────────────────── UDP ───────────────────────────────────

/// Arguments of `udp-socket.send`.
#[repr(C)]
struct SendToArgs {
  socket: i32,
  data: *const u8,
  len: usize,
  /// `1` when a remote address follows.
  has_remote: u8,
  remote: AddressAbi,
}

#[cfg(target_pointer_width = "32")]
const _: () = {
  assert!(size_of::<SendToArgs>() == 48);
  assert!(core::mem::offset_of!(SendToArgs, data) == 4);
  assert!(core::mem::offset_of!(SendToArgs, len) == 8);
  assert!(core::mem::offset_of!(SendToArgs, has_remote) == 12);
  assert!(core::mem::offset_of!(SendToArgs, remote) == 16);
};

/// Sends one datagram, like `sendto`.
///
/// `remote` is the destination, or `None` on a connected socket. The host
/// may trap if `data` is longer than 64 KiB.
pub struct SendTo<'a> {
  args: SendToArgs,
  reply: Slot<(), ErrorCode>,
  borrows: PhantomData<(&'a UdpSocket, &'a [u8])>,
}

impl<'a> SendTo<'a> {
  pub fn new(
    socket: &'a UdpSocket,
    data: &'a [u8],
    remote: Option<IpSocketAddress>,
  ) -> Self {
    Self {
      args: SendToArgs {
        socket: socket.handle().cast_signed(),
        data: data.as_ptr(),
        len: data.len(),
        has_remote: u8::from(remote.is_some()),
        remote: remote.map_or(AddressAbi::zeroed(), AddressAbi::lower),
      },
      reply: Slot::new(),
      borrows: PhantomData,
    }
  }
}

call!(SendTo => (), |op, reply| {
  imports::udp_send((&raw const op.args).cast(), reply)
});

/// `result<tuple<list<u8>, ip-socket-address>, error-code>` as the host
/// writes it.
#[repr(C)]
struct ReceiveReply {
  /// `0` for ok, `1` for error, [`NO_RESULT`] otherwise.
  tag: u8,
  /// Ok: the datagram's pointer. Error: the error code.
  a: usize,
  /// Ok: the datagram's length. Error: whether a message follows.
  b: usize,
  /// Ok: the sender's address. Error: the message's pointer and length.
  tail: AddressAbi,
}

#[cfg(target_pointer_width = "32")]
const _: () = {
  assert!(size_of::<ReceiveReply>() == 44);
  assert!(core::mem::offset_of!(ReceiveReply, a) == 4);
  assert!(core::mem::offset_of!(ReceiveReply, b) == 8);
  assert!(core::mem::offset_of!(ReceiveReply, tail) == 12);
};

impl ReceiveReply {
  /// The words an error is lifted from.
  fn error_words(&self) -> Words {
    Words {
      a: self.a,
      b: self.b,
      c: self.tail.u32_at(0) as usize as *mut u8,
      d: self.tail.u32_at(4) as usize,
    }
  }

  fn take(&mut self) -> Option<Result<(Vec<u8>, IpSocketAddress), ErrorCode>> {
    match core::mem::replace(&mut self.tag, NO_RESULT) {
      NO_RESULT => None,
      0 => {
        let data = if self.b == 0 {
          Vec::new()
        } else {
          // SAFETY: the host allocated exactly `b` bytes with alignment 1
          // through this component's allocator and handed them over. The
          // tag was cleared, so they are adopted once.
          unsafe { Vec::from_raw_parts(self.a as *mut u8, self.b, self.b) }
        };
        Some(Ok((data, self.tail.lift())))
      }
      // SAFETY: the host wrote an error, and the tag was cleared, so it is
      // lifted once.
      1 => Some(Err(unsafe { self.error_words().error() })),
      other => unreachable!("unknown result tag {other:#x}"),
    }
  }
}

/// Receives one datagram and its sender, like `recvfrom`.
///
/// The host allocates the datagram in this component; WASI 0.3 has no way
/// to receive one into caller memory.
pub struct ReceiveFrom<'a> {
  socket: i32,
  /// The host writes through a pointer into this.
  reply: UnsafeCell<ReceiveReply>,
  borrows: PhantomData<&'a UdpSocket>,
}

impl<'a> ReceiveFrom<'a> {
  pub fn new(socket: &'a UdpSocket) -> Self {
    Self {
      socket: socket.handle().cast_signed(),
      reply: UnsafeCell::new(ReceiveReply {
        tag: NO_RESULT,
        a: 0,
        b: 0,
        tail: AddressAbi::zeroed(),
      }),
      borrows: PhantomData,
    }
  }

  /// Takes the datagram and the address it came from.
  ///
  /// Returns `None` if the call has not returned, was cancelled, or its
  /// result was already taken.
  pub fn result(
    &mut self,
  ) -> Option<Result<(Vec<u8>, IpSocketAddress), ErrorCode>> {
    self.reply.get_mut().take()
  }
}

impl Drop for ReceiveFrom<'_> {
  fn drop(&mut self) {
    // An untaken result owns a datagram or a message. Release it.
    drop(self.result());
  }
}

impl Call for ReceiveFrom<'_> {}

impl sealed::Call for ReceiveFrom<'_> {
  unsafe fn start(&mut self) -> u32 {
    drop(self.result());
    // SAFETY: the caller keeps `self` and the socket in place until the
    // call returns.
    let packed =
      unsafe { imports::udp_receive(self.socket, self.reply.get().cast()) };
    packed as u32
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn error_cases_are_in_wit_order() {
    assert!(matches!(ErrorCode::plain(0), Some(ErrorCode::AccessDenied)));
    assert!(matches!(ErrorCode::plain(9), Some(ErrorCode::ConnectionRefused)));
    assert!(matches!(ErrorCode::plain(13), Some(ErrorCode::DatagramTooLarge)));
    assert!(ErrorCode::plain(14).is_none());
  }

  #[test]
  fn an_ipv4_address_is_laid_out_as_the_abi_has_it() {
    let abi = AddressAbi::lower(IpSocketAddress::Ipv4(Ipv4SocketAddress {
      port: 0x1234,
      address: (127, 0, 0, 1),
    }));
    assert_eq!(abi.0[0], 0);
    assert_eq!(&abi.0[4..10], &[0x34, 0x12, 127, 0, 0, 1]);
  }

  #[test]
  fn an_ipv6_address_is_laid_out_as_the_abi_has_it() {
    let abi = AddressAbi::lower(IpSocketAddress::Ipv6(Ipv6SocketAddress {
      port: 8080,
      flow_info: 0x0a0b_0c0d,
      address: (0x2001, 0xdb8, 0, 0, 0, 0, 0, 1),
      scope_id: 7,
    }));
    assert_eq!(abi.0[0], 1);
    assert_eq!(&abi.0[4..6], &8080u16.to_le_bytes());
    assert_eq!(&abi.0[8..12], &[0x0d, 0x0c, 0x0b, 0x0a]);
    assert_eq!(&abi.0[12..16], &[0x01, 0x20, 0xb8, 0x0d]);
    assert_eq!(&abi.0[26..28], &[1, 0]);
    assert_eq!(&abi.0[28..32], &[7, 0, 0, 0]);
  }

  #[test]
  fn addresses_survive_a_round_trip() {
    let v4 = IpSocketAddress::Ipv4(Ipv4SocketAddress {
      port: 53,
      address: (10, 20, 30, 40),
    });
    let IpSocketAddress::Ipv4(back) = AddressAbi::lower(v4).lift() else {
      panic!("expected IPv4");
    };
    assert_eq!((back.port, back.address), (53, (10, 20, 30, 40)));

    let v6 = IpSocketAddress::Ipv6(Ipv6SocketAddress {
      port: 443,
      flow_info: 99,
      address: (1, 2, 3, 4, 5, 6, 7, 8),
      scope_id: 11,
    });
    let IpSocketAddress::Ipv6(back) = AddressAbi::lower(v6).lift() else {
      panic!("expected IPv6");
    };
    assert_eq!((back.port, back.flow_info, back.scope_id), (443, 99, 11));
    assert_eq!(back.address, (1, 2, 3, 4, 5, 6, 7, 8));
  }

  #[test]
  fn an_empty_accept_slot_has_no_socket() {
    assert!(Accepted::new().socket().is_none());
  }

  #[test]
  fn a_received_datagram_is_adopted_with_its_sender() {
    let datagram = vec![1u8, 2, 3].into_boxed_slice();
    let len = datagram.len();
    let mut reply = ReceiveReply {
      tag: 0,
      a: Box::into_raw(datagram) as *mut u8 as usize,
      b: len,
      tail: AddressAbi::lower(IpSocketAddress::Ipv4(Ipv4SocketAddress {
        port: 9,
        address: (127, 0, 0, 1),
      })),
    };
    let Some(Ok((data, IpSocketAddress::Ipv4(from)))) = reply.take() else {
      panic!("expected a datagram from an IPv4 sender");
    };
    assert_eq!(data, [1, 2, 3]);
    assert_eq!(from.port, 9);
    assert!(reply.take().is_none());
  }

  #[test]
  fn an_empty_datagram_reads_no_pointer() {
    let mut reply =
      ReceiveReply { tag: 0, a: 0, b: 0, tail: AddressAbi::zeroed() };
    let Some(Ok((data, _))) = reply.take() else {
      panic!("expected an empty datagram");
    };
    assert!(data.is_empty());
  }

  #[test]
  fn a_receive_error_is_lifted() {
    let mut reply =
      ReceiveReply { tag: 1, a: 9, b: 0, tail: AddressAbi::zeroed() };
    assert!(matches!(reply.take(), Some(Err(ErrorCode::ConnectionRefused))));
  }
}
