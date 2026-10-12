#![allow(
  clippy::duplicate_mod,
  clippy::unnecessary_mut_passed,
  clippy::expect_fun_call
)]

//! Tests for recvfrom operations.

use super::common::{
  TcpPair, as_tcp_stream, poll_recv, poll_until_recv, setup_tcp_pair,
  socket_resource,
};
use lio::Lio;
use lio::api;
use lio::api::resource::Resource;
use std::io::Write;
use std::net::{SocketAddr, UdpSocket};
use std::sync::mpsc;

struct UdpPair {
  receiver: Resource,
  sender: UdpSocket,
  sender_addr: SocketAddr,
}

fn setup_udp_pair() -> UdpPair {
  let receiver =
    UdpSocket::bind("127.0.0.1:0").expect("failed to bind receiver socket");
  let recv_addr = receiver.local_addr().expect("failed to query receiver");
  let receiver = socket_resource(receiver);

  let sender =
    UdpSocket::bind("127.0.0.1:0").expect("failed to bind sender socket");
  let sender_addr =
    sender.local_addr().expect("failed to query sender address");
  sender.connect(recv_addr).expect("failed to connect sender socket");

  UdpPair { receiver, sender, sender_addr }
}

fn send_all(fd: &Resource, data: &[u8]) {
  as_tcp_stream(fd).write_all(data).expect("peer send failed");
}

#[cfg(unix)]
fn setup_unix_dgram_pair() -> (Resource, Resource) {
  use std::os::unix::net::UnixDatagram;
  let (a, b) = UnixDatagram::pair().expect("socketpair failed");
  (socket_resource(a), socket_resource(b))
}

#[test]
fn basic() {
  let mut lio = Lio::new(64).unwrap();
  let UdpPair { receiver, sender, sender_addr } = setup_udp_pair();

  let data = b"Hello, Datagram!".to_vec();
  sender.send(&data).expect("peer send failed");

  let mut receiver_op =
    api::recvfrom(&receiver, vec![0u8; 1024], None).with_lio(&mut lio).send();

  let (bytes_received, received_buf, from_addr): (
    std::io::Result<i32>,
    Vec<u8>,
    Option<SocketAddr>,
  ) = poll_recv(&mut lio, &mut receiver_op);
  let bytes_received = bytes_received.expect("recvfrom failed") as usize;

  assert_eq!(bytes_received, data.len());
  assert_eq!(&received_buf[..bytes_received], data.as_slice());
  assert_eq!(from_addr, Some(sender_addr));
}

#[test]
fn multiple() {
  let mut lio = Lio::new(64).unwrap();
  let UdpPair { receiver, sender, sender_addr } = setup_udp_pair();

  for i in 0..10 {
    let data = format!("Datagram {}", i).into_bytes();
    sender.send(&data).expect("peer send failed");

    let (sender_recv, receiver_recv) = mpsc::channel();
    api::recvfrom(&receiver, vec![0u8; 64], None)
      .with_lio(&mut lio)
      .send_with(sender_recv);

    let (bytes_received, received_buf, from_addr): (
      std::io::Result<i32>,
      Vec<u8>,
      Option<SocketAddr>,
    ) = poll_until_recv(&mut lio, &receiver_recv);
    let bytes_received = bytes_received.expect("recvfrom failed") as usize;

    assert_eq!(bytes_received, data.len());
    assert_eq!(&received_buf[..bytes_received], data.as_slice());
    assert_eq!(from_addr, Some(sender_addr));
  }
}

#[test]
fn partial_buffer() {
  let mut lio = Lio::new(64).unwrap();
  let UdpPair { receiver, sender, sender_addr } = setup_udp_pair();

  let data = b"This datagram is larger than the receive buffer".to_vec();
  sender.send(&data).expect("peer send failed");

  let (sender_recv, receiver_recv) = mpsc::channel();
  api::recvfrom(&receiver, vec![0u8; 10], None)
    .with_lio(&mut lio)
    .send_with(sender_recv);

  let (bytes_received, received_buf, from_addr): (
    std::io::Result<i32>,
    Vec<u8>,
    Option<SocketAddr>,
  ) = poll_until_recv(&mut lio, &receiver_recv);
  let bytes_received = bytes_received.expect("recvfrom failed") as usize;

  assert!(bytes_received <= 10);
  assert_eq!(&received_buf[..bytes_received], &data[..bytes_received]);
  assert_eq!(from_addr, Some(sender_addr));
}

#[test]
fn with_flags() {
  let mut lio = Lio::new(64).unwrap();
  let UdpPair { receiver, sender, sender_addr } = setup_udp_pair();

  let data = b"flags datagram".to_vec();
  sender.send(&data).expect("peer send failed");

  let (sender_recv, receiver_recv) = mpsc::channel();
  api::recvfrom(&receiver, vec![0u8; 64], None)
    .with_lio(&mut lio)
    .send_with(sender_recv);

  let (bytes_received, received_buf, from_addr): (
    std::io::Result<i32>,
    Vec<u8>,
    Option<SocketAddr>,
  ) = poll_until_recv(&mut lio, &receiver_recv);
  let bytes_received =
    bytes_received.expect("recvfrom with flags failed") as usize;

  assert_eq!(bytes_received, data.len());
  assert_eq!(&received_buf[..bytes_received], data.as_slice());
  assert_eq!(from_addr, Some(sender_addr));
}

#[test]
fn tcp_stream() {
  let mut lio = Lio::new(64).unwrap();
  let TcpPair { server_sock: _, client_sock, accepted_fd } =
    setup_tcp_pair(&mut lio);

  let data = b"tcp recvfrom".to_vec();
  send_all(&client_sock, &data);

  let mut receiver_op = api::recvfrom(&accepted_fd, vec![0u8; 1024], None)
    .with_lio(&mut lio)
    .send();

  let (bytes_received, received_buf, from_addr): (
    std::io::Result<i32>,
    Vec<u8>,
    Option<SocketAddr>,
  ) = poll_recv(&mut lio, &mut receiver_op);
  let bytes_received = bytes_received.expect("recvfrom failed on TCP") as usize;

  assert_eq!(bytes_received, data.len());
  assert_eq!(&received_buf[..bytes_received], data.as_slice());
  assert_eq!(from_addr, None);
}

#[cfg(unix)]
#[test]
fn unix_dgram() {
  let mut lio = Lio::new(64).unwrap();
  let (receiver, sender) = setup_unix_dgram_pair();

  let data = b"unix recvfrom".to_vec();
  send_all(&sender, &data);

  let mut receiver_op =
    api::recvfrom(&receiver, vec![0u8; 1024], None).with_lio(&mut lio).send();

  let (bytes_received, received_buf, from_addr): (
    std::io::Result<i32>,
    Vec<u8>,
    Option<SocketAddr>,
  ) = poll_recv(&mut lio, &mut receiver_op);
  let bytes_received =
    bytes_received.expect("recvfrom failed on Unix dgram") as usize;

  assert_eq!(bytes_received, data.len());
  assert_eq!(&received_buf[..bytes_received], data.as_slice());
  assert_eq!(from_addr, None);
}
