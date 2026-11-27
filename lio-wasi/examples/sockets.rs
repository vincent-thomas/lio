//! Runs TCP and UDP over loopback through a [`lio_wasi::WaitableSet`], with
//! both ends of each conversation in this one component.
//!
//! ```text
//! cargo build -p lio-wasi --example sockets --target wasm32-wasip3
//! wasmtime run -S p3 -S inherit-network \
//!   target/wasm32-wasip3/debug/examples/sockets.wasm
//! ```

use lio_wasi::sockets::{
  Accepted, Connect, ErrorCode, IpAddressFamily, IpSocketAddress,
  Ipv4SocketAddress, ReceiveFrom, SendTo, StreamStatus, TcpSocket, UdpSocket,
};
use lio_wasi::{
  Call, Cancel, Event, ReadStream, ReadableFuture, Started, StreamResult,
  WaitableSet, WriteStream,
};

mod common;

use common::Out;

fn loopback(port: u16) -> IpSocketAddress {
  IpSocketAddress::Ipv4(Ipv4SocketAddress { port, address: (127, 0, 0, 1) })
}

fn port_of(address: IpSocketAddress) -> u16 {
  match address {
    IpSocketAddress::Ipv4(v4) => v4.port,
    IpSocketAddress::Ipv6(v6) => v6.port,
  }
}

fn must<T>(what: &str, result: Result<T, ErrorCode>) -> T {
  match result {
    Ok(value) => value,
    Err(error) => panic!("{what} failed: {error:?}"),
  }
}

/// Makes `call` and waits for it to return.
fn run<C: Call>(set: &mut WaitableSet, call: &mut C) {
  // SAFETY: `call` is borrowed for this whole function, which waits for
  // the call to return.
  if let Some(subtask) = unsafe { set.start(call) } {
    assert_eq!(set.wait(), Some(Event::Returned(subtask)));
  }
}

/// Reads a stream's status and waits for it to be delivered.
fn read_status<F: ReadableFuture>(set: &mut WaitableSet, status: &mut F) {
  // SAFETY: `status` is borrowed for this whole function, which waits for
  // the value to be delivered.
  if let Some(future) = unsafe { set.read_future(status) } {
    assert_eq!(set.wait(), Some(Event::FutureRead(future)));
  }
}

/// Reads into `buf` and waits for the read to end.
fn read(
  set: &mut WaitableSet,
  stream: ReadStream,
  buf: &mut [u8],
) -> StreamResult {
  // SAFETY: `buf` is borrowed for this whole function, which waits for the
  // read to end.
  match unsafe { set.read(stream, buf.as_mut_ptr(), buf.len()) } {
    Ok(Started::Done(result)) => result,
    Ok(Started::Blocked) => match set.wait() {
      Some(Event::Read(ended, result)) if ended == stream => result,
      other => unreachable!("only the read is in flight: {other:?}"),
    },
    Err(error) => panic!("the read was refused: {error}"),
  }
}

/// Writes from `data` and waits for the write to end.
fn write(
  set: &mut WaitableSet,
  stream: WriteStream,
  data: &[u8],
) -> StreamResult {
  // SAFETY: `data` is borrowed for this whole function, which waits for
  // the write to end.
  match unsafe { set.write(stream, data.as_ptr(), data.len()) } {
    Ok(Started::Done(result)) => result,
    Ok(Started::Blocked) => match set.wait() {
      Some(Event::Write(ended, result)) if ended == stream => result,
      other => unreachable!("only the write is in flight: {other:?}"),
    },
    Err(error) => panic!("the write was refused: {error}"),
  }
}

/// One side of a TCP connection: its two byte streams and their statuses.
struct Side {
  input: ReadStream,
  input_status: StreamStatus,
  output: WriteStream,
  output_status: StreamStatus,
}

impl Side {
  fn new(set: &mut WaitableSet, socket: &TcpSocket) -> Self {
    let (input, input_status) = socket.receive();
    let (writer, reader) = lio_wasi::stream();
    let output_status = socket.send(reader);
    Self {
      input: set.add_reader(input),
      input_status: StreamStatus::new(input_status),
      output: set.add_writer(writer),
      output_status: StreamStatus::new(output_status),
    }
  }

  fn send(&self, set: &mut WaitableSet, data: &[u8]) {
    let mut sent = 0;
    while sent < data.len() {
      let done = write(set, self.output, &data[sent..]);
      assert!(
        matches!(done, StreamResult::Completed(_)),
        "send ended: {done:?}"
      );
      sent += done.count();
    }
  }

  /// Reads until `want` bytes have arrived or the stream ends.
  fn receive(&self, set: &mut WaitableSet, want: usize) -> (Vec<u8>, bool) {
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    let mut closed = false;
    while out.len() < want && !closed {
      let done = read(set, self.input, &mut buf);
      out.extend_from_slice(&buf[..done.count()]);
      closed = matches!(done, StreamResult::Dropped(_));
    }
    (out, closed)
  }
}

fn tcp(set: &mut WaitableSet, out: &mut Out) {
  let listener = must("create", TcpSocket::create(IpAddressFamily::Ipv4));
  must("bind", listener.bind(loopback(0)));
  let connections = must("listen", listener.listen());
  let port = port_of(must("local address", listener.get_local_address()));
  let connections = set.add_listener(connections);

  // An accept with nobody connecting stays in flight, and cancels cleanly.
  let mut accepted = Accepted::new();
  // SAFETY: `accepted` stays put until each accept has ended.
  let accept = unsafe { set.accept(connections, &mut accepted) };
  assert_eq!(accept, Ok(Started::Blocked));
  assert!(set.poll().is_none());
  assert_eq!(set.cancel_accept(connections), Some(StreamResult::Cancelled(0)));
  assert_eq!(set.cancel_accept(connections), None);
  assert!(accepted.socket().is_none());

  // Accept and connect in flight together.
  let client = must("create", TcpSocket::create(IpAddressFamily::Ipv4));
  let mut connect = Connect::new(&client, loopback(port));
  // SAFETY: `accepted` and `connect` stay put until both have ended, which
  // the loop below waits for.
  let (accept, connecting) = unsafe {
    (set.accept(connections, &mut accepted), set.start(&mut connect))
  };
  let mut arrived = match accept {
    Ok(Started::Done(result)) => Some(result),
    Ok(Started::Blocked) => None,
    Err(error) => panic!("the accept was refused: {error}"),
  };
  let mut connected = connecting.is_none();
  while let Some(event) = set.wait() {
    match event {
      Event::Accept(listener, result) if listener == connections => {
        arrived = Some(result);
      }
      Event::Returned(subtask) if Some(subtask) == connecting => {
        connected = true;
      }
      other => unreachable!("an event for nothing started here: {other:?}"),
    }
  }
  assert_eq!(arrived, Some(StreamResult::Completed(1)));
  assert!(connected, "the connect never returned");
  must("connect", connect.result().unwrap_or(Err(ErrorCode::InvalidState)));
  let Some(server) = accepted.socket() else {
    panic!("the accept delivered no socket");
  };
  assert!(accepted.socket().is_none(), "a connection is taken once");
  let peer = port_of(must("remote address", server.get_remote_address()));
  let local = port_of(must("local address", client.get_local_address()));
  assert_eq!(peer, local);
  say!(out, "tcp accept, connect: ok (port {port})");

  // Echo in both directions, with more data than one read holds.
  let near = Side::new(set, &client);
  let far = Side::new(set, &server);
  near.send(set, b"ping");
  assert_eq!(far.receive(set, 4).0, b"ping");
  far.send(set, b"pong");
  assert_eq!(near.receive(set, 4).0, b"pong");
  let big: Vec<u8> = (0..20_000u32).map(|n| (n % 251) as u8).collect();
  near.send(set, &big);
  assert_eq!(far.receive(set, big.len()).0, big);
  say!(out, "tcp send, receive: ok");

  // Closing the client's writer shuts down its write side: the server reads
  // to the end, and both statuses report a clean close.
  let Side { input: near_in, output: near_out, .. } = near;
  let (mut near_in_status, mut near_out_status) =
    (near.input_status, near.output_status);
  assert_eq!(set.close_writer(near_out), Ok(None));
  let (rest, closed) = far.receive(set, usize::MAX);
  assert!(rest.is_empty() && closed);
  read_status(set, &mut near_out_status);
  assert!(matches!(near_out_status.result(), Some(Ok(()))));
  let Side { input: far_in, output: far_out, .. } = far;
  let (mut far_in_status, mut far_out_status) =
    (far.input_status, far.output_status);
  read_status(set, &mut far_in_status);
  assert!(matches!(far_in_status.result(), Some(Ok(()))));
  assert_eq!(set.close_writer(far_out), Ok(None));
  read_status(set, &mut far_out_status);
  assert!(matches!(far_out_status.result(), Some(Ok(()))));
  let mut buf = [0u8; 8];
  assert_eq!(read(set, near_in, &mut buf), StreamResult::Dropped(0));
  read_status(set, &mut near_in_status);
  assert!(matches!(near_in_status.result(), Some(Ok(()))));
  assert_eq!(set.close_reader(near_in), Ok(None));
  assert_eq!(set.close_reader(far_in), Ok(None));
  say!(out, "tcp shutdown, stream status: ok");

  // Connecting to a port nobody listens on fails with a reason.
  assert_eq!(set.close_listener(connections), Ok(None));
  drop(listener);
  let stray = must("create", TcpSocket::create(IpAddressFamily::Ipv4));
  let mut refused = Connect::new(&stray, loopback(port));
  run(set, &mut refused);
  let Some(Err(error)) = refused.result() else {
    panic!("connected to a closed port");
  };
  say!(out, "tcp connect to a closed port: {error:?}");
}

fn udp(set: &mut WaitableSet, out: &mut Out) {
  let a = must("create", UdpSocket::create(IpAddressFamily::Ipv4));
  let b = must("create", UdpSocket::create(IpAddressFamily::Ipv4));
  must("bind", a.bind(loopback(0)));
  must("bind", b.bind(loopback(0)));
  let a_port = port_of(must("local address", a.get_local_address()));
  let b_port = port_of(must("local address", b.get_local_address()));

  // A receive with nothing sent stays in flight until a datagram arrives.
  let mut receive = ReceiveFrom::new(&b);
  // SAFETY: `receive` stays put until it has returned.
  let Some(receiving) = (unsafe { set.start(&mut receive) }) else {
    panic!("a receive returned with nothing sent");
  };
  assert!(set.poll().is_none());
  let mut send = SendTo::new(&a, b"datagram", Some(loopback(b_port)));
  // SAFETY: `send` stays put until it has returned.
  let sending = unsafe { set.start(&mut send) };
  let mut received = false;
  while let Some(event) = set.wait() {
    match event {
      Event::Returned(subtask) if subtask == receiving => received = true,
      Event::Returned(subtask) if Some(subtask) == sending => {}
      other => unreachable!("an event for nothing started here: {other:?}"),
    }
  }
  assert!(received, "the receive never returned");
  must("send", send.result().unwrap_or(Err(ErrorCode::InvalidState)));
  let Some(Ok((data, from))) = receive.result() else {
    panic!("the receive delivered no datagram");
  };
  assert_eq!(data, b"datagram");
  assert_eq!(port_of(from), a_port);
  assert!(receive.result().is_none(), "a datagram is taken once");
  say!(out, "udp send, receive: ok");

  // On a connected socket the destination is implied.
  must("connect", b.connect(loopback(a_port)));
  let mut reply = SendTo::new(&b, b"reply", None);
  run(set, &mut reply);
  must("send", reply.result().unwrap_or(Err(ErrorCode::InvalidState)));
  let mut receive = ReceiveFrom::new(&a);
  run(set, &mut receive);
  let Some(Ok((data, from))) = receive.result() else {
    panic!("the receive delivered no datagram");
  };
  assert_eq!((data.as_slice(), port_of(from)), (&b"reply"[..], b_port));
  say!(out, "udp connected send: ok");

  // A receive nobody answers is cancelled and leaves no result.
  let mut idle = ReceiveFrom::new(&a);
  // SAFETY: `idle` stays put until it is cancelled.
  let Some(waiting) = (unsafe { set.start(&mut idle) }) else {
    panic!("a receive returned with nothing sent");
  };
  assert_eq!(set.cancel(waiting), Some(Cancel::Cancelled));
  assert!(set.wait().is_none());
  assert!(idle.result().is_none());
  say!(out, "udp cancelled receive: ok");
}

fn main() {
  let mut set = WaitableSet::new();
  let mut out = Out::stdout();
  tcp(&mut set, &mut out);
  udp(&mut set, &mut out);
  assert_eq!(set.in_flight(), 0);
}
