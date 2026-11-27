//! `wasi:sockets`. What can wait (connect, accept, send, receive) is a lio
//! operation. Creating, binding and configuring a socket never waits and is
//! a direct system call.

use std::future::Future;
use std::net::{
  IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6,
  ToSocketAddrs,
};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration as StdDuration;

use lio::api;
use lio::api::ops::Accept;
use lio::api::resource::Resource;
use rustix::net::sockopt;
use rustix::net::{AddressFamily, Shutdown, SocketType};
use wasmtime::StoreContextMut;
use wasmtime::component::{
  Access, Accessor, Destination, FutureReader, Resource as Handle,
  StreamProducer, StreamReader, StreamResult,
};

use crate::bindings::wasi::sockets::ip_name_lookup;
use crate::bindings::wasi::sockets::types::{
  self, Duration, ErrorCode, IpAddress, IpAddressFamily, IpSocketAddress,
  Ipv4SocketAddress, Ipv6SocketAddress,
};
use crate::io::{Op, op, op_waits, socket_error};
use crate::stream::{ByteReader, ByteWriter, ReadFrom, Status, WriteTo};
use crate::{LioWasi, WasiCtx};

type Sock<T> = wasmtime::Result<Result<T, ErrorCode>>;

/// The largest datagram UDP carries.
const MAX_DATAGRAM: usize = 64 * 1024;

#[derive(Clone, Copy, PartialEq, Eq)]
enum TcpState {
  Unbound,
  Bound,
  Listening,
  Connecting,
  Connected,
}

/// A TCP socket.
pub struct TcpSocket {
  resource: Resource,
  family: IpAddressFamily,
  state: TcpState,
  backlog: i32,
  /// Each direction's stream can be asked for once.
  receiving: bool,
  sending: bool,
}

/// A UDP socket.
pub struct UdpSocket {
  resource: Resource,
  family: IpAddressFamily,
  connected: Option<SocketAddr>,
}

fn errno(error: rustix::io::Errno) -> ErrorCode {
  socket_error(&std::io::Error::from(error))
}

fn to_std(address: IpSocketAddress) -> SocketAddr {
  match address {
    IpSocketAddress::Ipv4(v4) => {
      let (a, b, c, d) = v4.address;
      SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(a, b, c, d), v4.port))
    }
    IpSocketAddress::Ipv6(v6) => {
      let (a, b, c, d, e, f, g, h) = v6.address;
      SocketAddr::V6(SocketAddrV6::new(
        Ipv6Addr::new(a, b, c, d, e, f, g, h),
        v6.port,
        v6.flow_info,
        v6.scope_id,
      ))
    }
  }
}

fn from_std(address: SocketAddr) -> IpSocketAddress {
  match address {
    SocketAddr::V4(v4) => {
      let [a, b, c, d] = v4.ip().octets();
      IpSocketAddress::Ipv4(Ipv4SocketAddress {
        port: v4.port(),
        address: (a, b, c, d),
      })
    }
    SocketAddr::V6(v6) => {
      let [a, b, c, d, e, f, g, h] = v6.ip().segments();
      IpSocketAddress::Ipv6(Ipv6SocketAddress {
        port: v6.port(),
        flow_info: v6.flowinfo(),
        address: (a, b, c, d, e, f, g, h),
        scope_id: v6.scope_id(),
      })
    }
  }
}

fn from_ip(address: IpAddr) -> IpAddress {
  match address {
    IpAddr::V4(v4) => {
      let [a, b, c, d] = v4.octets();
      IpAddress::Ipv4((a, b, c, d))
    }
    IpAddr::V6(v6) => {
      let [a, b, c, d, e, f, g, h] = v6.segments();
      IpAddress::Ipv6((a, b, c, d, e, f, g, h))
    }
  }
}

/// The address, if it is of the socket's family.
fn address(
  family: IpAddressFamily,
  address: IpSocketAddress,
) -> Result<SocketAddr, ErrorCode> {
  let address = to_std(address);
  let matches = match family {
    IpAddressFamily::Ipv4 => address.is_ipv4(),
    IpAddressFamily::Ipv6 => address.is_ipv6(),
  };
  if matches { Ok(address) } else { Err(ErrorCode::InvalidArgument) }
}

fn socket(
  family: IpAddressFamily,
  kind: SocketType,
) -> Result<Resource, ErrorCode> {
  let domain = match family {
    IpAddressFamily::Ipv4 => AddressFamily::INET,
    IpAddressFamily::Ipv6 => AddressFamily::INET6,
  };
  let socket = rustix::net::socket(domain, kind, None).map_err(errno)?;
  if family == IpAddressFamily::Ipv6 {
    // WASI's IPv6 sockets do not speak IPv4.
    sockopt::set_ipv6_v6only(&socket, true).map_err(errno)?;
  }
  let flags = rustix::fs::fcntl_getfl(&socket).map_err(errno)?;
  rustix::fs::fcntl_setfl(&socket, flags | rustix::fs::OFlags::NONBLOCK)
    .map_err(errno)?;
  Ok(Resource::from(socket))
}

fn name(resource: &Resource, peer: bool) -> Result<IpSocketAddress, ErrorCode> {
  let found = if peer {
    rustix::net::getpeername(resource)
  } else {
    rustix::net::getsockname(resource).map(Some)
  };
  let address = match found {
    Ok(Some(address)) => address,
    Ok(None) => return Err(ErrorCode::InvalidState),
    Err(rustix::io::Errno::NOTCONN) => return Err(ErrorCode::InvalidState),
    Err(error) => return Err(errno(error)),
  };
  match SocketAddr::try_from(address) {
    Ok(address) => Ok(from_std(address)),
    Err(_) => Err(ErrorCode::NotSupported),
  }
}

fn shut_read(resource: &Resource) {
  let _ = rustix::net::shutdown(resource, Shutdown::Read);
}

fn shut_write(resource: &Resource) {
  let _ = rustix::net::shutdown(resource, Shutdown::Write);
}

/// A status that is already known.
fn ended(how: Result<(), ErrorCode>) -> Status<ErrorCode> {
  Status::ready(how)
}

// ──────────────────────────────── accepting ─────────────────────────────────

/// The host end of a listener's `stream<tcp-socket>`.
struct Acceptor<D> {
  listener: Resource,
  family: IpAddressFamily,
  /// The accept in flight. It outlives a cancelled guest read, so a
  /// connection it takes is given to the next one.
  accepting: Option<Op<Accept>>,
  ctx: fn(&mut D) -> &mut WasiCtx,
}

impl<D: 'static> StreamProducer<D> for Acceptor<D> {
  type Item = Handle<TcpSocket>;
  type Buffer = Option<Handle<TcpSocket>>;

  fn poll_produce<'a>(
    self: Pin<&mut Self>,
    cx: &mut Context<'_>,
    mut store: StoreContextMut<'a, D>,
    mut dst: Destination<'a, Self::Item, Self::Buffer>,
    finish: bool,
  ) -> Poll<wasmtime::Result<StreamResult>> {
    let me = self.get_mut();
    if dst.remaining(&mut store)? == Some(0) {
      return Poll::Ready(Ok(StreamResult::Completed));
    }
    let accepting =
      me.accepting.get_or_insert_with(|| op_waits(api::accept(&me.listener)));
    let accepted = match Pin::new(accepting).poll(cx) {
      Poll::Ready(accepted) => accepted,
      Poll::Pending if finish => {
        return Poll::Ready(Ok(StreamResult::Cancelled));
      }
      Poll::Pending => return Poll::Pending,
    };
    me.accepting = None;
    let resource = match accepted {
      Ok((resource, _peer)) => resource,
      // The listener is unusable: the stream ends.
      Err(_) => return Poll::Ready(Ok(StreamResult::Dropped)),
    };
    let socket = TcpSocket {
      resource,
      family: me.family,
      state: TcpState::Connected,
      backlog: 0,
      receiving: false,
      sending: false,
    };
    let handle = (me.ctx)(store.data_mut()).table.push(socket)?;
    dst.set_buffer(Some(handle));
    Poll::Ready(Ok(StreamResult::Completed))
  }
}

// ─────────────────────────────────── TCP ────────────────────────────────────

impl types::Host for WasiCtx {}

impl WasiCtx {
  fn tcp(
    &mut self,
    socket: &Handle<TcpSocket>,
  ) -> wasmtime::Result<&mut TcpSocket> {
    Ok(self.table.get_mut(socket)?)
  }

  fn udp(
    &mut self,
    socket: &Handle<UdpSocket>,
  ) -> wasmtime::Result<&mut UdpSocket> {
    Ok(self.table.get_mut(socket)?)
  }
}

fn nanos(duration: StdDuration) -> Duration {
  u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

/// A keep-alive time in whole seconds, as the OS takes it.
fn seconds(value: Duration) -> Result<StdDuration, ErrorCode> {
  if value == 0 {
    return Err(ErrorCode::InvalidArgument);
  }
  let seconds = value.div_ceil(1_000_000_000).min(i16::MAX as u64);
  Ok(StdDuration::from_secs(seconds))
}

fn buffer_size(value: u64) -> Result<usize, ErrorCode> {
  if value == 0 {
    return Err(ErrorCode::InvalidArgument);
  }
  Ok(usize::try_from(value).unwrap_or(usize::MAX).min(i32::MAX as usize))
}

fn hop_limit(
  resource: &Resource,
  family: IpAddressFamily,
) -> Result<u8, ErrorCode> {
  match family {
    IpAddressFamily::Ipv4 => sockopt::ip_ttl(resource)
      .map(|ttl| u8::try_from(ttl).unwrap_or(u8::MAX))
      .map_err(errno),
    IpAddressFamily::Ipv6 => {
      sockopt::ipv6_unicast_hops(resource).map_err(errno)
    }
  }
}

fn set_hop_limit(
  resource: &Resource,
  family: IpAddressFamily,
  value: u8,
) -> Result<(), ErrorCode> {
  if value == 0 {
    return Err(ErrorCode::InvalidArgument);
  }
  match family {
    IpAddressFamily::Ipv4 => {
      sockopt::set_ip_ttl(resource, u32::from(value)).map_err(errno)
    }
    IpAddressFamily::Ipv6 => {
      sockopt::set_ipv6_unicast_hops(resource, Some(value)).map_err(errno)
    }
  }
}

impl types::HostTcpSocket for WasiCtx {
  fn create(&mut self, family: IpAddressFamily) -> Sock<Handle<TcpSocket>> {
    if !self.network {
      return Ok(Err(ErrorCode::AccessDenied));
    }
    let resource = match socket(family, SocketType::STREAM) {
      Ok(resource) => resource,
      Err(error) => return Ok(Err(error)),
    };
    let socket = TcpSocket {
      resource,
      family,
      state: TcpState::Unbound,
      backlog: 128,
      receiving: false,
      sending: false,
    };
    Ok(Ok(self.table.push(socket)?))
  }

  fn bind(
    &mut self,
    this: Handle<TcpSocket>,
    local: IpSocketAddress,
  ) -> Sock<()> {
    let socket = self.tcp(&this)?;
    Ok((|| {
      if socket.state != TcpState::Unbound {
        return Err(ErrorCode::InvalidState);
      }
      let local = address(socket.family, local)?;
      // A listener may rebind an address whose last user is lingering.
      sockopt::set_socket_reuseaddr(&socket.resource, true).map_err(errno)?;
      rustix::net::bind(&socket.resource, &local).map_err(
        |error| match error {
          rustix::io::Errno::AFNOSUPPORT => ErrorCode::InvalidArgument,
          other => errno(other),
        },
      )?;
      socket.state = TcpState::Bound;
      Ok(())
    })())
  }

  fn get_local_address(
    &mut self,
    this: Handle<TcpSocket>,
  ) -> Sock<IpSocketAddress> {
    let socket = self.tcp(&this)?;
    Ok(if socket.state == TcpState::Unbound {
      Err(ErrorCode::InvalidState)
    } else {
      name(&socket.resource, false)
    })
  }

  fn get_remote_address(
    &mut self,
    this: Handle<TcpSocket>,
  ) -> Sock<IpSocketAddress> {
    let socket = self.tcp(&this)?;
    Ok(if socket.state == TcpState::Connected {
      name(&socket.resource, true)
    } else {
      Err(ErrorCode::InvalidState)
    })
  }

  fn get_is_listening(
    &mut self,
    this: Handle<TcpSocket>,
  ) -> wasmtime::Result<bool> {
    Ok(self.tcp(&this)?.state == TcpState::Listening)
  }

  fn get_address_family(
    &mut self,
    this: Handle<TcpSocket>,
  ) -> wasmtime::Result<IpAddressFamily> {
    Ok(self.tcp(&this)?.family)
  }

  fn set_listen_backlog_size(
    &mut self,
    this: Handle<TcpSocket>,
    value: u64,
  ) -> Sock<()> {
    let socket = self.tcp(&this)?;
    Ok((|| {
      if value == 0 {
        return Err(ErrorCode::InvalidArgument);
      }
      let backlog = i32::try_from(value).unwrap_or(i32::MAX);
      match socket.state {
        TcpState::Unbound | TcpState::Bound => {}
        TcpState::Listening => {
          rustix::net::listen(&socket.resource, backlog).map_err(errno)?;
        }
        TcpState::Connecting | TcpState::Connected => {
          return Err(ErrorCode::InvalidState);
        }
      }
      socket.backlog = backlog;
      Ok(())
    })())
  }

  fn get_keep_alive_enabled(&mut self, this: Handle<TcpSocket>) -> Sock<bool> {
    let socket = self.tcp(&this)?;
    Ok(sockopt::socket_keepalive(&socket.resource).map_err(errno))
  }

  fn set_keep_alive_enabled(
    &mut self,
    this: Handle<TcpSocket>,
    value: bool,
  ) -> Sock<()> {
    let socket = self.tcp(&this)?;
    Ok(sockopt::set_socket_keepalive(&socket.resource, value).map_err(errno))
  }

  fn get_keep_alive_idle_time(
    &mut self,
    this: Handle<TcpSocket>,
  ) -> Sock<Duration> {
    let socket = self.tcp(&this)?;
    Ok(sockopt::tcp_keepidle(&socket.resource).map(nanos).map_err(errno))
  }

  fn set_keep_alive_idle_time(
    &mut self,
    this: Handle<TcpSocket>,
    value: Duration,
  ) -> Sock<()> {
    let socket = self.tcp(&this)?;
    Ok(seconds(value).and_then(|idle| {
      sockopt::set_tcp_keepidle(&socket.resource, idle).map_err(errno)
    }))
  }

  fn get_keep_alive_interval(
    &mut self,
    this: Handle<TcpSocket>,
  ) -> Sock<Duration> {
    let socket = self.tcp(&this)?;
    Ok(sockopt::tcp_keepintvl(&socket.resource).map(nanos).map_err(errno))
  }

  fn set_keep_alive_interval(
    &mut self,
    this: Handle<TcpSocket>,
    value: Duration,
  ) -> Sock<()> {
    let socket = self.tcp(&this)?;
    Ok(seconds(value).and_then(|interval| {
      sockopt::set_tcp_keepintvl(&socket.resource, interval).map_err(errno)
    }))
  }

  fn get_keep_alive_count(&mut self, this: Handle<TcpSocket>) -> Sock<u32> {
    let socket = self.tcp(&this)?;
    Ok(sockopt::tcp_keepcnt(&socket.resource).map_err(errno))
  }

  fn set_keep_alive_count(
    &mut self,
    this: Handle<TcpSocket>,
    value: u32,
  ) -> Sock<()> {
    let socket = self.tcp(&this)?;
    Ok(if value == 0 {
      Err(ErrorCode::InvalidArgument)
    } else {
      let count = value.min(i8::MAX as u32);
      sockopt::set_tcp_keepcnt(&socket.resource, count).map_err(errno)
    })
  }

  fn get_hop_limit(&mut self, this: Handle<TcpSocket>) -> Sock<u8> {
    let socket = self.tcp(&this)?;
    Ok(hop_limit(&socket.resource, socket.family))
  }

  fn set_hop_limit(&mut self, this: Handle<TcpSocket>, value: u8) -> Sock<()> {
    let socket = self.tcp(&this)?;
    Ok(set_hop_limit(&socket.resource, socket.family, value))
  }

  fn get_receive_buffer_size(&mut self, this: Handle<TcpSocket>) -> Sock<u64> {
    let socket = self.tcp(&this)?;
    Ok(
      sockopt::socket_recv_buffer_size(&socket.resource)
        .map(|size| size as u64)
        .map_err(errno),
    )
  }

  fn set_receive_buffer_size(
    &mut self,
    this: Handle<TcpSocket>,
    value: u64,
  ) -> Sock<()> {
    let socket = self.tcp(&this)?;
    Ok(buffer_size(value).and_then(|size| {
      sockopt::set_socket_recv_buffer_size(&socket.resource, size)
        .map_err(errno)
    }))
  }

  fn get_send_buffer_size(&mut self, this: Handle<TcpSocket>) -> Sock<u64> {
    let socket = self.tcp(&this)?;
    Ok(
      sockopt::socket_send_buffer_size(&socket.resource)
        .map(|size| size as u64)
        .map_err(errno),
    )
  }

  fn set_send_buffer_size(
    &mut self,
    this: Handle<TcpSocket>,
    value: u64,
  ) -> Sock<()> {
    let socket = self.tcp(&this)?;
    Ok(buffer_size(value).and_then(|size| {
      sockopt::set_socket_send_buffer_size(&socket.resource, size)
        .map_err(errno)
    }))
  }

  fn drop(&mut self, rep: Handle<TcpSocket>) -> wasmtime::Result<()> {
    self.table.delete(rep)?;
    Ok(())
  }
}

impl<T: Send + 'static> types::HostTcpSocketWithStore<T> for LioWasi {
  async fn connect(
    accessor: &Accessor<T, Self>,
    this: Handle<TcpSocket>,
    remote: IpSocketAddress,
  ) -> Sock<()> {
    let started = accessor.with(|mut store| {
      let socket = store.get().tcp(&this)?;
      Ok::<_, wasmtime::Error>((|| {
        if !matches!(socket.state, TcpState::Unbound | TcpState::Bound) {
          return Err(ErrorCode::InvalidState);
        }
        let remote = address(socket.family, remote)?;
        if remote.ip().is_unspecified() || remote.port() == 0 {
          return Err(ErrorCode::InvalidArgument);
        }
        socket.state = TcpState::Connecting;
        Ok((socket.resource.clone(), remote))
      })())
    })?;
    let (resource, remote) = match started {
      Ok(started) => started,
      Err(error) => return Ok(Err(error)),
    };
    let connected = op_waits(api::connect(&resource, remote)).await;
    accessor.with(|mut store| {
      // The guest may have dropped the socket while it connected.
      if let Ok(socket) = store.get().tcp(&this) {
        socket.state = match connected {
          Ok(()) => TcpState::Connected,
          Err(_) => TcpState::Unbound,
        };
      }
    });
    Ok(connected.map_err(|e| socket_error(&e)))
  }

  fn listen(
    mut host: Access<T, Self>,
    this: Handle<TcpSocket>,
  ) -> Sock<StreamReader<Handle<TcpSocket>>> {
    let ctx = host.getter();
    let socket = host.get().tcp(&this)?;
    let listening = (|| {
      if socket.state != TcpState::Bound {
        return Err(ErrorCode::InvalidState);
      }
      rustix::net::listen(&socket.resource, socket.backlog).map_err(errno)?;
      socket.state = TcpState::Listening;
      Ok((socket.resource.clone(), socket.family))
    })();
    let (listener, family) = match listening {
      Ok(listening) => listening,
      Err(error) => return Ok(Err(error)),
    };
    let acceptor = Acceptor { listener, family, accepting: None, ctx };
    Ok(Ok(StreamReader::new(&mut host, acceptor)?))
  }

  fn send(
    mut host: Access<T, Self>,
    this: Handle<TcpSocket>,
    mut data: StreamReader<u8>,
  ) -> wasmtime::Result<FutureReader<Result<(), ErrorCode>>> {
    let socket = host.get().tcp(&this)?;
    if socket.state != TcpState::Connected || socket.sending {
      data.close(&mut host)?;
      return FutureReader::new(&mut host, ended(Err(ErrorCode::InvalidState)));
    }
    socket.sending = true;
    let to = WriteTo::Socket(socket.resource.clone());
    let lending = host.get().lending.clone();
    let (writer, status) =
      ByteWriter::new(to, socket_error, shut_write, lending);
    data.pipe(&mut host, writer)?;
    FutureReader::new(&mut host, status)
  }

  fn receive(
    mut host: Access<T, Self>,
    this: Handle<TcpSocket>,
  ) -> wasmtime::Result<(StreamReader<u8>, FutureReader<Result<(), ErrorCode>>)>
  {
    let socket = host.get().tcp(&this)?;
    if socket.state != TcpState::Connected || socket.receiving {
      return Ok((
        StreamReader::new(&mut host, std::iter::empty())?,
        FutureReader::new(&mut host, ended(Err(ErrorCode::InvalidState)))?,
      ));
    }
    socket.receiving = true;
    let from = ReadFrom::Socket(socket.resource.clone());
    let lending = host.get().lending.clone();
    let (reader, status) =
      ByteReader::new(from, socket_error, shut_read, lending);
    Ok((
      StreamReader::new(&mut host, reader)?,
      FutureReader::new(&mut host, status)?,
    ))
  }
}

// ─────────────────────────────────── UDP ────────────────────────────────────

impl types::HostUdpSocket for WasiCtx {
  fn create(&mut self, family: IpAddressFamily) -> Sock<Handle<UdpSocket>> {
    if !self.network {
      return Ok(Err(ErrorCode::AccessDenied));
    }
    let resource = match socket(family, SocketType::DGRAM) {
      Ok(resource) => resource,
      Err(error) => return Ok(Err(error)),
    };
    let socket = UdpSocket { resource, family, connected: None };
    Ok(Ok(self.table.push(socket)?))
  }

  fn bind(
    &mut self,
    this: Handle<UdpSocket>,
    local: IpSocketAddress,
  ) -> Sock<()> {
    let socket = self.udp(&this)?;
    Ok(address(socket.family, local).and_then(|local| {
      rustix::net::bind(&socket.resource, &local).map_err(errno)
    }))
  }

  fn connect(
    &mut self,
    this: Handle<UdpSocket>,
    remote: IpSocketAddress,
  ) -> Sock<()> {
    let socket = self.udp(&this)?;
    Ok((|| {
      let remote = address(socket.family, remote)?;
      if remote.ip().is_unspecified() || remote.port() == 0 {
        return Err(ErrorCode::InvalidArgument);
      }
      rustix::net::connect(&socket.resource, &remote).map_err(errno)?;
      socket.connected = Some(remote);
      Ok(())
    })())
  }

  fn disconnect(&mut self, this: Handle<UdpSocket>) -> Sock<()> {
    let socket = self.udp(&this)?;
    Ok(if socket.connected.take().is_none() {
      Err(ErrorCode::InvalidState)
    } else {
      match rustix::net::connect_unspec(&socket.resource) {
        // Dissolving the association reports the family it no longer has.
        Ok(()) | Err(rustix::io::Errno::AFNOSUPPORT) => Ok(()),
        Err(error) => Err(errno(error)),
      }
    })
  }

  fn get_local_address(
    &mut self,
    this: Handle<UdpSocket>,
  ) -> Sock<IpSocketAddress> {
    let socket = self.udp(&this)?;
    Ok(match name(&socket.resource, false) {
      // An unbound socket has the unspecified address and no port.
      Ok(IpSocketAddress::Ipv4(v4)) if v4.port == 0 => {
        Err(ErrorCode::InvalidState)
      }
      Ok(IpSocketAddress::Ipv6(v6)) if v6.port == 0 => {
        Err(ErrorCode::InvalidState)
      }
      other => other,
    })
  }

  fn get_remote_address(
    &mut self,
    this: Handle<UdpSocket>,
  ) -> Sock<IpSocketAddress> {
    let socket = self.udp(&this)?;
    Ok(socket.connected.map(from_std).ok_or(ErrorCode::InvalidState))
  }

  fn get_address_family(
    &mut self,
    this: Handle<UdpSocket>,
  ) -> wasmtime::Result<IpAddressFamily> {
    Ok(self.udp(&this)?.family)
  }

  fn get_unicast_hop_limit(&mut self, this: Handle<UdpSocket>) -> Sock<u8> {
    let socket = self.udp(&this)?;
    Ok(hop_limit(&socket.resource, socket.family))
  }

  fn set_unicast_hop_limit(
    &mut self,
    this: Handle<UdpSocket>,
    value: u8,
  ) -> Sock<()> {
    let socket = self.udp(&this)?;
    Ok(set_hop_limit(&socket.resource, socket.family, value))
  }

  fn get_receive_buffer_size(&mut self, this: Handle<UdpSocket>) -> Sock<u64> {
    let socket = self.udp(&this)?;
    Ok(
      sockopt::socket_recv_buffer_size(&socket.resource)
        .map(|size| size as u64)
        .map_err(errno),
    )
  }

  fn set_receive_buffer_size(
    &mut self,
    this: Handle<UdpSocket>,
    value: u64,
  ) -> Sock<()> {
    let socket = self.udp(&this)?;
    Ok(buffer_size(value).and_then(|size| {
      sockopt::set_socket_recv_buffer_size(&socket.resource, size)
        .map_err(errno)
    }))
  }

  fn get_send_buffer_size(&mut self, this: Handle<UdpSocket>) -> Sock<u64> {
    let socket = self.udp(&this)?;
    Ok(
      sockopt::socket_send_buffer_size(&socket.resource)
        .map(|size| size as u64)
        .map_err(errno),
    )
  }

  fn set_send_buffer_size(
    &mut self,
    this: Handle<UdpSocket>,
    value: u64,
  ) -> Sock<()> {
    let socket = self.udp(&this)?;
    Ok(buffer_size(value).and_then(|size| {
      sockopt::set_socket_send_buffer_size(&socket.resource, size)
        .map_err(errno)
    }))
  }

  fn drop(&mut self, rep: Handle<UdpSocket>) -> wasmtime::Result<()> {
    self.table.delete(rep)?;
    Ok(())
  }
}

impl<T: Send + 'static> types::HostUdpSocketWithStore<T> for LioWasi {
  async fn send(
    accessor: &Accessor<T, Self>,
    this: Handle<UdpSocket>,
    data: Vec<u8>,
    remote: Option<IpSocketAddress>,
  ) -> Sock<()> {
    let (resource, family, connected) = accessor.with(|mut store| {
      let socket = store.get().udp(&this)?;
      Ok::<_, wasmtime::Error>((
        socket.resource.clone(),
        socket.family,
        socket.connected,
      ))
    })?;
    if data.len() > MAX_DATAGRAM {
      return Ok(Err(ErrorCode::DatagramTooLarge));
    }
    let len = data.len();
    let (sent, _data) = match (remote, connected) {
      (Some(remote), connected) => {
        let remote = match address(family, remote) {
          Ok(remote) => remote,
          Err(error) => return Ok(Err(error)),
        };
        // A connected socket sends only to its peer.
        if connected.is_some_and(|peer| peer != remote) {
          return Ok(Err(ErrorCode::InvalidArgument));
        }
        if connected.is_some() {
          op(api::send(&resource, data, None)).await
        } else {
          op(api::sendto(&resource, data, remote, None)).await
        }
      }
      (None, Some(_)) => op(api::send(&resource, data, None)).await,
      (None, None) => return Ok(Err(ErrorCode::InvalidArgument)),
    };
    Ok(match sent {
      Ok(sent) if sent == len => Ok(()),
      Ok(_) => Err(ErrorCode::DatagramTooLarge),
      Err(error) => Err(socket_error(&error)),
    })
  }

  async fn receive(
    accessor: &Accessor<T, Self>,
    this: Handle<UdpSocket>,
  ) -> Sock<(Vec<u8>, IpSocketAddress)> {
    let (resource, connected) = accessor.with(|mut store| {
      let socket = store.get().udp(&this)?;
      Ok::<_, wasmtime::Error>((socket.resource.clone(), socket.connected))
    })?;
    let buf = Vec::with_capacity(MAX_DATAGRAM);
    let (received, buf, from) =
      op_waits(api::recvfrom(&resource, buf, None)).await;
    Ok(match (received, from.or(connected)) {
      (Ok(_), Some(from)) => Ok((buf, from_std(from))),
      (Ok(_), None) => {
        Err(ErrorCode::Other(Some(String::from("the datagram has no sender"))))
      }
      (Err(error), _) => Err(socket_error(&error)),
    })
  }
}

// ─────────────────────────────── name lookup ────────────────────────────────

impl ip_name_lookup::Host for WasiCtx {}

impl<T: Send + 'static> ip_name_lookup::HostWithStore<T> for LioWasi {
  async fn resolve_addresses(
    accessor: &Accessor<T, Self>,
    name: String,
  ) -> wasmtime::Result<Result<Vec<IpAddress>, ip_name_lookup::ErrorCode>> {
    use ip_name_lookup::ErrorCode as LookupError;
    if !accessor.with(|mut store| store.get().name_lookup) {
      return Ok(Err(LookupError::AccessDenied));
    }
    if let Ok(address) = name.parse::<IpAddr>() {
      return Ok(Ok(vec![from_ip(address)]));
    }
    if name.is_empty() || name.contains(|c: char| c.is_whitespace() || c == '/')
    {
      return Ok(Err(LookupError::InvalidArgument));
    }
    // lio has no resolver: this asks the system's, and blocks while it
    // answers.
    Ok(match (name.as_str(), 0).to_socket_addrs() {
      Ok(found) => {
        let mut addresses: Vec<IpAddr> = Vec::new();
        for address in found {
          if !addresses.contains(&address.ip()) {
            addresses.push(address.ip());
          }
        }
        Ok(addresses.into_iter().map(from_ip).collect())
      }
      Err(_) => Err(LookupError::NameUnresolvable),
    })
  }
}
