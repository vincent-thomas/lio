//! Runs TCP and UDP over loopback through a [`lio_wasi::Queue`], with both
//! ends of each conversation in this one component.
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
use lio_wasi::{Call, Completion, Outcome, Queue, ReadStream, WriteStream};

const ACCEPT: u64 = 1;
const CONNECT: u64 = 2;
const READ: u64 = 3;
const WRITE: u64 = 4;

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

fn next(queue: &mut Queue) -> Completion {
  let Some(done) = queue.wait() else {
    unreachable!("an operation is outstanding")
  };
  done
}

/// Pushes `call` and waits for it to return.
fn run<C: Call>(queue: &mut Queue, call: &mut C) {
  // SAFETY: `call` is borrowed for this whole function, which takes the
  // completion before returning.
  unsafe { queue.push_call(call, 42) };
  let done = next(queue);
  assert_eq!((done.user_data(), done.outcome()), (42, Outcome::Returned));
}

/// One side of a TCP connection: its two byte streams and their statuses.
struct Side {
  input: ReadStream,
  input_status: StreamStatus,
  output: WriteStream,
  output_status: StreamStatus,
}

impl Side {
  fn new(queue: &mut Queue, socket: &TcpSocket) -> Self {
    let (input, input_status) = socket.receive();
    let (writer, reader) = wasip3::wit_stream::new::<u8>();
    let output_status = socket.send(reader);
    Self {
      input: queue.add_reader(input),
      input_status: StreamStatus::new(input_status),
      output: queue.add_writer(writer),
      output_status: StreamStatus::new(output_status),
    }
  }

  fn send(&self, queue: &mut Queue, data: &[u8]) {
    let mut sent = 0;
    while sent < data.len() {
      let rest = &data[sent..];
      // SAFETY: `data` outlives the completion taken on the next line.
      let pushed = unsafe {
        queue.push_write(self.output, rest.as_ptr(), rest.len(), WRITE)
      };
      assert_eq!(pushed, Ok(()));
      let done = next(queue).outcome();
      assert!(matches!(done, Outcome::Transferred(_)), "send ended: {done:?}");
      sent += done.count();
    }
  }

  /// Reads until `want` bytes have arrived or the stream ends.
  fn receive(&self, queue: &mut Queue, want: usize) -> (Vec<u8>, bool) {
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    let mut closed = false;
    while out.len() < want && !closed {
      // SAFETY: `buf` outlives the completion taken on the next line.
      let pushed = unsafe {
        queue.push_read(self.input, buf.as_mut_ptr(), buf.len(), READ)
      };
      assert_eq!(pushed, Ok(()));
      let done = next(queue).outcome();
      out.extend_from_slice(&buf[..done.count()]);
      closed = matches!(done, Outcome::Closed(_));
    }
    (out, closed)
  }
}

fn tcp(queue: &mut Queue) {
  let listener = must("create", TcpSocket::create(IpAddressFamily::Ipv4));
  must("bind", listener.bind(loopback(0)));
  let connections = must("listen", listener.listen());
  let port = port_of(must("local address", listener.get_local_address()));
  let connections = queue.add_listener(connections);

  // An accept with nobody connecting stays in flight, and cancels cleanly.
  let mut accepted = Accepted::new();
  // SAFETY: `accepted` stays put until each accept's completion is taken.
  unsafe {
    assert_eq!(queue.push_accept(connections, &mut accepted, ACCEPT), Ok(()));
  }
  assert!(queue.poll().is_none());
  assert!(queue.cancel(ACCEPT));
  assert_eq!(next(queue).outcome(), Outcome::Cancelled(0));
  assert!(accepted.socket().is_none());

  // Accept and connect in flight together.
  let client = must("create", TcpSocket::create(IpAddressFamily::Ipv4));
  let mut connect = Connect::new(&client, loopback(port));
  // SAFETY: `accepted` and `connect` stay put until both complete.
  unsafe {
    assert_eq!(queue.push_accept(connections, &mut accepted, ACCEPT), Ok(()));
    queue.push_call(&mut connect, CONNECT);
  }
  let mut seen = [next(queue), next(queue)];
  seen.sort_by_key(Completion::user_data);
  assert_eq!(
    (seen[0].user_data(), seen[0].outcome()),
    (ACCEPT, Outcome::Transferred(1))
  );
  assert_eq!(
    (seen[1].user_data(), seen[1].outcome()),
    (CONNECT, Outcome::Returned)
  );
  must("connect", connect.result().unwrap_or(Err(ErrorCode::InvalidState)));
  let Some(server) = accepted.socket() else {
    panic!("the accept delivered no socket");
  };
  assert!(accepted.socket().is_none(), "a connection is taken once");
  let peer = port_of(must("remote address", server.get_remote_address()));
  let local = port_of(must("local address", client.get_local_address()));
  assert_eq!(peer, local);
  println!("tcp accept, connect: ok (port {port})");

  // Echo in both directions, with more data than one read holds.
  let near = Side::new(queue, &client);
  let far = Side::new(queue, &server);
  near.send(queue, b"ping");
  assert_eq!(far.receive(queue, 4).0, b"ping");
  far.send(queue, b"pong");
  assert_eq!(near.receive(queue, 4).0, b"pong");
  let big: Vec<u8> = (0..20_000u32).map(|n| (n % 251) as u8).collect();
  near.send(queue, &big);
  assert_eq!(far.receive(queue, big.len()).0, big);
  println!("tcp send, receive: ok");

  // Closing the client's writer shuts down its write side: the server reads
  // to the end, and both statuses report a clean close.
  let Side { input: near_in, output: near_out, .. } = near;
  let (mut near_in_status, mut near_out_status) =
    (near.input_status, near.output_status);
  assert_eq!(queue.close_writer(near_out), Ok(()));
  let (rest, closed) = far.receive(queue, usize::MAX);
  assert!(rest.is_empty() && closed);
  run(queue, &mut near_out_status);
  assert!(matches!(near_out_status.result(), Some(Ok(()))));
  let Side { input: far_in, output: far_out, .. } = far;
  let (mut far_in_status, mut far_out_status) =
    (far.input_status, far.output_status);
  run(queue, &mut far_in_status);
  assert!(matches!(far_in_status.result(), Some(Ok(()))));
  assert_eq!(queue.close_writer(far_out), Ok(()));
  run(queue, &mut far_out_status);
  assert!(matches!(far_out_status.result(), Some(Ok(()))));
  let mut buf = [0u8; 8];
  // SAFETY: `buf` outlives the completion taken on the next line.
  unsafe {
    assert_eq!(
      queue.push_read(near_in, buf.as_mut_ptr(), buf.len(), READ),
      Ok(())
    );
  }
  assert_eq!(next(queue).outcome(), Outcome::Closed(0));
  run(queue, &mut near_in_status);
  assert!(matches!(near_in_status.result(), Some(Ok(()))));
  assert_eq!(queue.close_reader(near_in), Ok(()));
  assert_eq!(queue.close_reader(far_in), Ok(()));
  println!("tcp shutdown, stream status: ok");

  // Connecting to a port nobody listens on fails with a reason.
  assert_eq!(queue.close_listener(connections), Ok(()));
  drop(listener);
  let stray = must("create", TcpSocket::create(IpAddressFamily::Ipv4));
  let mut refused = Connect::new(&stray, loopback(port));
  run(queue, &mut refused);
  let Some(Err(error)) = refused.result() else {
    panic!("connected to a closed port");
  };
  println!("tcp connect to a closed port: {error:?}");
}

fn udp(queue: &mut Queue) {
  let a = must("create", UdpSocket::create(IpAddressFamily::Ipv4));
  let b = must("create", UdpSocket::create(IpAddressFamily::Ipv4));
  must("bind", a.bind(loopback(0)));
  must("bind", b.bind(loopback(0)));
  let a_port = port_of(must("local address", a.get_local_address()));
  let b_port = port_of(must("local address", b.get_local_address()));

  // A receive with nothing sent stays in flight until a datagram arrives.
  let mut receive = ReceiveFrom::new(&b);
  // SAFETY: `receive` stays put until its completion is taken.
  unsafe { queue.push_call(&mut receive, READ) };
  assert!(queue.poll().is_none());
  let mut send = SendTo::new(&a, b"datagram", Some(loopback(b_port)));
  // SAFETY: `send` stays put until its completion is taken.
  unsafe { queue.push_call(&mut send, WRITE) };
  let mut seen = [next(queue), next(queue)];
  seen.sort_by_key(Completion::user_data);
  assert_eq!(seen[0].outcome(), Outcome::Returned);
  assert_eq!(seen[1].outcome(), Outcome::Returned);
  must("send", send.result().unwrap_or(Err(ErrorCode::InvalidState)));
  let Some(Ok((data, from))) = receive.result() else {
    panic!("the receive delivered no datagram");
  };
  assert_eq!(data, b"datagram");
  assert_eq!(port_of(from), a_port);
  assert!(receive.result().is_none(), "a datagram is taken once");
  println!("udp send, receive: ok");

  // On a connected socket the destination is implied.
  must("connect", b.connect(loopback(a_port)));
  let mut reply = SendTo::new(&b, b"reply", None);
  run(queue, &mut reply);
  must("send", reply.result().unwrap_or(Err(ErrorCode::InvalidState)));
  let mut receive = ReceiveFrom::new(&a);
  run(queue, &mut receive);
  let Some(Ok((data, from))) = receive.result() else {
    panic!("the receive delivered no datagram");
  };
  assert_eq!((data.as_slice(), port_of(from)), (&b"reply"[..], b_port));
  println!("udp connected send: ok");

  // A receive nobody answers is cancelled and leaves no result.
  let mut idle = ReceiveFrom::new(&a);
  // SAFETY: `idle` stays put until its completion is taken.
  unsafe { queue.push_call(&mut idle, READ) };
  assert!(queue.cancel(READ));
  assert_eq!(next(queue).outcome(), Outcome::Cancelled(0));
  assert!(idle.result().is_none());
  println!("udp cancelled receive: ok");
}

fn main() {
  let mut queue = Queue::new();
  tcp(&mut queue);
  udp(&mut queue);
  assert_eq!(queue.outstanding(), 0);
}
