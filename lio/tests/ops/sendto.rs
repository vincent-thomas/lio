#![allow(
  clippy::duplicate_mod,
  clippy::unnecessary_mut_passed,
  clippy::expect_fun_call
)]

//! Tests for sendto operations.

use super::common::{
  TcpPair, as_tcp_stream, poll_recv, poll_until_recv, setup_tcp_pair,
  socket_resource,
};
use lio::Lio;
use lio::api;
use lio::api::resource::Resource;
use std::io::Read;
use std::net::{SocketAddr, UdpSocket};
use std::sync::mpsc;

/// "Already connected": `EISCONN` / `WSAEISCONN`.
#[cfg(unix)]
const EISCONN: i32 = libc::EISCONN;
#[cfg(windows)]
const EISCONN: i32 = 10056;

struct UdpPair {
  sender: Resource,
  receiver: UdpSocket,
  receiver_addr: SocketAddr,
}

fn setup_udp_pair() -> UdpPair {
  let sender = socket_resource(
    UdpSocket::bind("127.0.0.1:0").expect("failed to bind sender socket"),
  );
  let receiver =
    UdpSocket::bind("127.0.0.1:0").expect("failed to bind receiver socket");
  let receiver_addr =
    receiver.local_addr().expect("failed to query receiver address");

  UdpPair { sender, receiver, receiver_addr }
}

fn recv_once(socket: &UdpSocket) -> (Vec<u8>, SocketAddr) {
  let mut buf = vec![0u8; 64 * 1024];
  let (n, addr) = socket.recv_from(&mut buf).expect("peer recv_from failed");
  buf.truncate(n);
  (buf, addr)
}

fn recv_all(fd: &Resource, len: usize) -> Vec<u8> {
  let mut out = vec![0u8; len];
  let mut read = 0usize;
  let mut stream = as_tcp_stream(fd);

  while read < len {
    let n = stream.read(&mut out[read..]).expect("peer recv failed");
    if n == 0 {
      break;
    }
    read += n;
  }

  out.truncate(read);
  out
}

fn get_peer_addr(sock: &Resource) -> SocketAddr {
  as_tcp_stream(sock).peer_addr().expect("getpeername failed")
}

#[test]
fn basic() {
  let mut lio = Lio::new(64).unwrap();
  let UdpPair { sender, receiver, receiver_addr } = setup_udp_pair();

  let data = b"Hello, Receiver!".to_vec();
  let mut sender_op = api::sendto(&sender, data.clone(), receiver_addr, None)
    .with_lio(&mut lio)
    .send();

  let (bytes_sent, returned_buf) = poll_recv(&mut lio, &mut sender_op);
  let bytes_sent = bytes_sent.expect("sendto failed");

  assert_eq!(bytes_sent, data.len());
  assert_eq!(returned_buf, data);

  let (received, _from_addr) = recv_once(&receiver);
  assert_eq!(received, data);
}

#[test]
fn multiple() {
  let mut lio = Lio::new(64).unwrap();
  let UdpPair { sender, receiver, receiver_addr } = setup_udp_pair();

  for i in 0..10 {
    let data = format!("Message {}", i).into_bytes();
    let expected = data.clone();

    let (sender_send, receiver_send) = mpsc::channel();
    api::sendto(&sender, data, receiver_addr, None)
      .with_lio(&mut lio)
      .send_with(sender_send);

    let (bytes_sent, returned_buf) = poll_until_recv(&mut lio, &receiver_send);
    let bytes_sent = bytes_sent.expect("sendto failed");

    assert_eq!(bytes_sent, expected.len());
    assert_eq!(returned_buf, expected);

    let (received, _from_addr) = recv_once(&receiver);
    assert_eq!(received, expected);
  }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
#[test]
fn with_flags() {
  let mut lio = Lio::new(64).unwrap();
  let UdpPair { sender, receiver, receiver_addr } = setup_udp_pair();

  let data = b"sendto with flags".to_vec();
  let expected = data.clone();

  let (sender_send, receiver_send) = mpsc::channel();
  api::sendto(
    &sender,
    data,
    receiver_addr,
    Some(api::SendFlags::from_bits(libc::MSG_NOSIGNAL).unwrap()),
  )
  .with_lio(&mut lio)
  .send_with(sender_send);

  let (bytes_sent, returned_buf) = poll_until_recv(&mut lio, &receiver_send);
  let bytes_sent = bytes_sent.expect("sendto with flags failed") as usize;

  assert_eq!(bytes_sent, expected.len());
  assert_eq!(returned_buf, expected);

  let (received, _from_addr) = recv_once(&receiver);
  assert_eq!(received, expected);
}

#[test]
fn large_data() {
  let mut lio = Lio::new(64).unwrap();
  let UdpPair { sender, receiver, receiver_addr } = setup_udp_pair();

  let data: Vec<u8> = (0..8192).map(|i| (i % 256) as u8).collect();
  let expected = data.clone();

  let mut sender_op =
    api::sendto(&sender, data, receiver_addr, None).with_lio(&mut lio).send();

  let (bytes_sent, returned_buf) = poll_recv(&mut lio, &mut sender_op);
  let bytes_sent = bytes_sent.expect("sendto large_data failed");

  assert_eq!(bytes_sent, expected.len());
  assert_eq!(returned_buf, expected);

  let (received, _from_addr) = recv_once(&receiver);
  assert_eq!(received, expected);
}

#[test]
fn tcp_stream() {
  let mut lio = Lio::new(64).unwrap();
  let TcpPair { server_sock: _, client_sock, accepted_fd } =
    setup_tcp_pair(&mut lio);

  let data = b"tcp sendto".to_vec();
  let peer_addr = get_peer_addr(&client_sock);
  let mut sender_op = api::sendto(&client_sock, data.clone(), peer_addr, None)
    .with_lio(&mut lio)
    .send();

  let (bytes_sent, returned_buf) = poll_recv(&mut lio, &mut sender_op);
  assert_eq!(returned_buf, data);

  match bytes_sent {
    Ok(n) => {
      assert_eq!(n, data.len());
      assert_eq!(recv_all(&accepted_fd, data.len()), data);
    }
    Err(err) => {
      let errno = err.raw_os_error();
      assert!(
        errno == Some(EISCONN),
        "unexpected sendto-on-TCP error: {err:?}"
      );
    }
  }
}
