#![allow(
  clippy::duplicate_mod,
  clippy::unnecessary_mut_passed,
  clippy::expect_fun_call
)]

//! Tests for send operations.

use super::common;
use super::common::{poll_recv, poll_until_recv, setup_tcp_pair};
use lio::Lio;
use lio::api;
use std::io::Read;
use std::sync::mpsc;

fn recv_all(fd: &lio::api::resource::Resource, len: usize) -> Vec<u8> {
  let mut out = vec![0u8; len];
  let mut read = 0usize;

  let mut stream = common::as_tcp_stream(fd);
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

#[test]
fn basic() {
  let mut lio = Lio::new(64).unwrap();
  let common::TcpPair { server_sock: _, client_sock, accepted_fd } =
    setup_tcp_pair(&mut lio);

  let data = b"Hello, Server!".to_vec();
  let mut sender =
    api::send(&client_sock, data.clone(), None).with_lio(&mut lio).send();

  let (bytes_sent, returned_buf) = poll_recv(&mut lio, &mut sender);
  let bytes_sent = bytes_sent.expect("Failed to send data");

  assert_eq!(bytes_sent, data.len());
  assert_eq!(returned_buf, data);
  assert_eq!(recv_all(&accepted_fd, data.len()), data);
}

#[test]
fn large_data() {
  let mut lio = Lio::new(64).unwrap();
  let common::TcpPair { server_sock: _, client_sock, accepted_fd } =
    setup_tcp_pair(&mut lio);

  let large_data: Vec<u8> = (0..64 * 1024).map(|i| (i % 256) as u8).collect();
  let expected = large_data.clone();
  let mut sender =
    api::send(&client_sock, large_data, None).with_lio(&mut lio).send();

  let (bytes_sent, returned_buf) = poll_recv(&mut lio, &mut sender);
  let bytes_sent = bytes_sent.expect("Failed to send large data");

  assert_eq!(bytes_sent, expected.len());
  assert_eq!(returned_buf, expected);
  assert_eq!(recv_all(&accepted_fd, expected.len()), expected);
}

#[test]
fn multiple() {
  let mut lio = Lio::new(64).unwrap();
  let common::TcpPair { server_sock: _, client_sock, accepted_fd } =
    setup_tcp_pair(&mut lio);

  for i in 0..10 {
    let data = format!("Message {}", i).into_bytes();
    let expected = data.clone();

    let (sender_send, receiver_send) = mpsc::channel();
    api::send(&client_sock, data, None)
      .with_lio(&mut lio)
      .send_with(sender_send);

    let (bytes_sent, returned_buf) = poll_until_recv(&mut lio, &receiver_send);
    let bytes_sent = bytes_sent.expect("Failed to send");

    assert_eq!(bytes_sent, expected.len());
    assert_eq!(returned_buf, expected);
    assert_eq!(recv_all(&accepted_fd, expected.len()), expected);
  }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
#[test]
fn with_flags() {
  let mut lio = Lio::new(64).unwrap();
  let common::TcpPair { server_sock: _, client_sock, accepted_fd } =
    setup_tcp_pair(&mut lio);

  let data = b"Data with flags".to_vec();
  let expected = data.clone();

  let (sender_send, receiver_send) = mpsc::channel();
  api::send(
    &client_sock,
    data,
    Some(api::SendFlags::from_bits(libc::MSG_NOSIGNAL).unwrap()),
  )
  .with_lio(&mut lio)
  .send_with(sender_send);

  let (bytes_sent, returned_buf) = poll_until_recv(&mut lio, &receiver_send);
  let bytes_sent = bytes_sent.expect("Failed to send with flags") as usize;

  assert_eq!(bytes_sent, expected.len());
  assert_eq!(returned_buf, expected);
  assert_eq!(recv_all(&accepted_fd, expected.len()), expected);
}

#[test]
fn concurrent_pairs() {
  let mut lio = Lio::new(256).unwrap();
  let mut pairs = Vec::new();
  let mut receivers = Vec::new();

  for _ in 0..5 {
    pairs.push(setup_tcp_pair(&mut lio));
  }

  let mut expected = Vec::new();

  for (i, pair) in pairs.iter().enumerate() {
    let data = format!("Client {} data", i).into_bytes();
    expected.push((i, data.clone()));
    let (sender_send, receiver_send) = mpsc::channel();
    api::send(&pair.client_sock, data, None)
      .with_lio(&mut lio)
      .send_with(sender_send);
    receivers.push(receiver_send);
  }

  for ((_, data), receiver) in expected.iter().zip(receivers.iter()) {
    let (res, returned) = poll_until_recv(&mut lio, receiver);
    assert_eq!(res.expect("Send should succeed"), data.len());
    assert_eq!(&returned, data);
  }

  for (idx, data) in expected {
    let received = recv_all(&pairs[idx].accepted_fd, data.len());
    assert_eq!(received, data);
  }
}
