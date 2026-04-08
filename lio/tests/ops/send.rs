//! Tests for send operations.

use super::common;
use super::common::{poll_recv, poll_until_recv, setup_tcp_pair};
use lio::Lio;
use lio::api;
use std::os::fd::{AsFd, AsRawFd};
use std::sync::mpsc;

fn recv_all(fd: &lio::api::resource::Resource, len: usize) -> Vec<u8> {
  let mut out = vec![0u8; len];
  let mut read = 0usize;

  while read < len {
    let n = unsafe {
      libc::recv(
        fd.as_fd().as_raw_fd(),
        out[read..].as_mut_ptr().cast(),
        out.len() - read,
        0,
      )
    };
    assert!(n >= 0, "peer recv failed");
    if n == 0 {
      break;
    }
    read += n as usize;
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
  let bytes_sent = bytes_sent.expect("Failed to send data") as usize;

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
  let bytes_sent = bytes_sent.expect("Failed to send large data") as usize;

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
    let bytes_sent = bytes_sent.expect("Failed to send") as usize;

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
  api::send(&client_sock, data, Some(libc::MSG_NOSIGNAL))
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

  for _ in 0..5 {
    pairs.push(setup_tcp_pair(&mut lio));
  }

  let (sender_send, receiver_send) = mpsc::channel();
  let mut expected = Vec::new();

  for (i, pair) in pairs.iter().enumerate() {
    let data = format!("Client {} data", i).into_bytes();
    expected.push((i, data.clone()));
    api::send(&pair.client_sock, data, None)
      .with_lio(&mut lio)
      .send_with(sender_send.clone());
  }

  for (_, data) in &expected {
    let (res, returned) = poll_until_recv(&mut lio, &receiver_send);
    assert_eq!(res.expect("Send should succeed") as usize, data.len());
    assert_eq!(&returned, data);
  }

  for (idx, data) in expected {
    let received = recv_all(&pairs[idx].accepted_fd, data.len());
    assert_eq!(received, data);
  }
}
