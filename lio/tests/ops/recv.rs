#![allow(
  clippy::duplicate_mod,
  clippy::unnecessary_mut_passed,
  clippy::expect_fun_call
)]

//! Tests for recv operations.

use super::common;
use super::common::{poll_recv, poll_until_recv, setup_tcp_pair};
use lio::Lio;
use lio::api;
use std::os::fd::{AsFd, AsRawFd};
use std::sync::mpsc;

fn send_all(fd: &lio::api::resource::Resource, data: &[u8]) {
  let mut written = 0usize;

  while written < data.len() {
    let n = unsafe {
      libc::send(
        fd.as_fd().as_raw_fd(),
        data[written..].as_ptr().cast(),
        data.len() - written,
        0,
      )
    };
    assert!(n >= 0, "peer send failed");
    written += n as usize;
  }
}

#[test]
fn basic() {
  let mut lio = Lio::new(64).unwrap();
  let common::TcpPair { server_sock: _, client_sock, accepted_fd } =
    setup_tcp_pair(&mut lio);

  let data = b"Hello, Server!".to_vec();
  send_all(&client_sock, &data);

  let mut receiver =
    api::recv(&accepted_fd, vec![0u8; 1024], None).with_lio(&mut lio).send();

  let (bytes_received, received_buf) = poll_recv(&mut lio, &mut receiver);
  let bytes_received = bytes_received.expect("Failed to receive data") as usize;

  assert_eq!(bytes_received, data.len());
  assert_eq!(&received_buf[..bytes_received], data.as_slice());
}

#[test]
fn large_data() {
  let mut lio = Lio::new(64).unwrap();
  let common::TcpPair { server_sock: _, client_sock, accepted_fd } =
    setup_tcp_pair(&mut lio);

  let large_data: Vec<u8> = (0..64 * 1024).map(|i| (i % 256) as u8).collect();
  let data_len = large_data.len();
  send_all(&client_sock, &large_data);

  let mut all_received = Vec::new();
  while all_received.len() < data_len {
    let (sender_recv, receiver_recv) = mpsc::channel();
    api::recv(&accepted_fd, vec![0u8; 8192], None)
      .with_lio(&mut lio)
      .send_with(sender_recv);

    let (bytes_received, received_buf) =
      poll_until_recv(&mut lio, &receiver_recv);
    let bytes_received = bytes_received.expect("Failed to receive") as usize;

    if bytes_received == 0 {
      break;
    }
    all_received.extend_from_slice(&received_buf[..bytes_received]);
  }

  assert_eq!(all_received.len(), data_len);
  assert_eq!(all_received, large_data);
}

#[test]
fn multiple() {
  let mut lio = Lio::new(64).unwrap();
  let common::TcpPair { server_sock: _, client_sock, accepted_fd } =
    setup_tcp_pair(&mut lio);

  for i in 0..10 {
    let data = format!("Message {}", i).into_bytes();
    send_all(&client_sock, &data);

    let (sender_recv, receiver_recv) = mpsc::channel();
    api::recv(&accepted_fd, vec![0u8; 64], None)
      .with_lio(&mut lio)
      .send_with(sender_recv);

    let (bytes_received, received_buf) =
      poll_until_recv(&mut lio, &receiver_recv);
    let bytes_received = bytes_received.expect("Failed to receive") as usize;

    assert_eq!(bytes_received, data.len());
    assert_eq!(&received_buf[..bytes_received], data.as_slice());
  }
}

#[test]
fn with_flags() {
  let mut lio = Lio::new(64).unwrap();
  let common::TcpPair { server_sock: _, client_sock, accepted_fd } =
    setup_tcp_pair(&mut lio);

  let data = b"Data with flags".to_vec();
  send_all(&client_sock, &data);

  let (sender_recv, receiver_recv) = mpsc::channel();
  api::recv(&accepted_fd, vec![0u8; 64], Some(0))
    .with_lio(&mut lio)
    .send_with(sender_recv);

  let (bytes_received, received_buf) =
    poll_until_recv(&mut lio, &receiver_recv);
  let bytes_received =
    bytes_received.expect("Failed to receive with flags") as usize;

  assert_eq!(bytes_received, data.len());
  assert_eq!(&received_buf[..bytes_received], data.as_slice());
}

#[test]
fn on_closed_conn() {
  let mut lio = Lio::new(64).unwrap();
  let common::TcpPair { server_sock: _, client_sock, accepted_fd } =
    setup_tcp_pair(&mut lio);

  drop(client_sock);

  let (sender_recv, receiver_recv) = mpsc::channel();
  api::recv(&accepted_fd, vec![0u8; 64], None)
    .with_lio(&mut lio)
    .send_with(sender_recv);

  let (bytes_received, _) = poll_until_recv(&mut lio, &receiver_recv);
  let bytes_received = bytes_received.expect("recv should succeed with 0");

  assert_eq!(bytes_received, 0);
}

#[test]
fn partial_buffer() {
  let mut lio = Lio::new(64).unwrap();
  let common::TcpPair { server_sock: _, client_sock, accepted_fd } =
    setup_tcp_pair(&mut lio);

  let data = b"This is a longer message that exceeds buffer".to_vec();
  send_all(&client_sock, &data);

  let (sender_recv, receiver_recv) = mpsc::channel();
  api::recv(&accepted_fd, vec![0u8; 10], None)
    .with_lio(&mut lio)
    .send_with(sender_recv);

  let (bytes_received, received_buf) =
    poll_until_recv(&mut lio, &receiver_recv);
  let bytes_received = bytes_received.expect("Failed to receive") as usize;

  assert!(bytes_received <= 10);
  assert_eq!(&received_buf[..bytes_received], &data[..bytes_received]);
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
    send_all(&pair.client_sock, &data);
    expected.push(data);
  }

  for pair in &pairs {
    let (sender_recv, receiver_recv) = mpsc::channel();
    api::recv(&pair.accepted_fd, vec![0u8; 64], None)
      .with_lio(&mut lio)
      .send_with(sender_recv);
    receivers.push(receiver_recv);
  }

  for (message, receiver) in expected.iter().zip(receivers.iter()) {
    let (res, buf) = poll_until_recv(&mut lio, receiver);
    let bytes = res.expect("Recv should succeed") as usize;
    assert_eq!(&buf[..bytes], message.as_slice());
  }
}
