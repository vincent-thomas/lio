//! Send/recv tests using the current lio API.

mod common;

use common::{poll_recv, poll_until_recv, setup_tcp_pair};
use lio::{Lio, api};
use std::sync::mpsc;

#[test]
fn test_send_recv_basic() {
  let mut lio = Lio::new(64).unwrap();

  let common::TcpPair { server_sock: _, client_sock, accepted_fd } =
    setup_tcp_pair(&mut lio);

  // Send data from client to server
  let data = b"Hello, Server!".to_vec();
  let mut send_recv =
    api::send(&client_sock, data.clone(), None).with_lio(&mut lio).send();

  let mut recv_recv =
    api::recv(&accepted_fd, vec![0u8; 1024], None).with_lio(&mut lio).send();

  let (bytes_sent, returned_buf) = poll_recv(&mut lio, &mut send_recv);
  let bytes_sent = bytes_sent.expect("Failed to send data");

  assert_eq!(bytes_sent as usize, data.len());
  assert_eq!(returned_buf, data);

  let (bytes_received, received_buf) = poll_recv(&mut lio, &mut recv_recv);
  let bytes_received = bytes_received.expect("Failed to receive data") as usize;

  assert_eq!(bytes_received, data.len());
  assert_eq!(&received_buf[..bytes_received], data.as_slice());
}

#[test]
fn test_send_recv_large_data() {
  let mut lio = Lio::new(64).unwrap();

  let common::TcpPair { server_sock: _, client_sock, accepted_fd } =
    setup_tcp_pair(&mut lio);

  // Send large data (64KB to stay reasonable)
  let large_data: Vec<u8> = (0..64 * 1024).map(|i| (i % 256) as u8).collect();
  let data_len = large_data.len();

  let (sender_send, receiver_send) = mpsc::channel();
  api::send(&client_sock, large_data.clone(), None)
    .with_lio(&mut lio)
    .send_with(sender_send);

  let (bytes_sent, returned_buf) = poll_until_recv(&mut lio, &receiver_send);
  let bytes_sent = bytes_sent.expect("Failed to send large data");

  assert!(bytes_sent > 0);
  assert_eq!(returned_buf, large_data);

  // Receive in chunks until we get all data
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
fn test_send_recv_multiple() {
  let mut lio = Lio::new(64).unwrap();

  let common::TcpPair { server_sock: _, client_sock, accepted_fd } =
    setup_tcp_pair(&mut lio);

  // Send 10 messages in sequence
  for i in 0..10 {
    let data = format!("Message {}", i).into_bytes();
    let data_len = data.len();

    let (sender_send, receiver_send) = mpsc::channel();
    api::send(&client_sock, data.clone(), None)
      .with_lio(&mut lio)
      .send_with(sender_send);

    let (bytes_sent, returned_buf) = poll_until_recv(&mut lio, &receiver_send);
    let bytes_sent = bytes_sent.expect("Failed to send") as usize;

    assert_eq!(bytes_sent, data_len);
    assert_eq!(returned_buf, data);

    // Receive the message
    let (sender_recv, receiver_recv) = mpsc::channel();
    api::recv(&accepted_fd, vec![0u8; 64], None)
      .with_lio(&mut lio)
      .send_with(sender_recv);

    let (bytes_received, received_buf) =
      poll_until_recv(&mut lio, &receiver_recv);
    let bytes_received = bytes_received.expect("Failed to receive") as usize;

    assert_eq!(bytes_received, data_len);
    assert_eq!(&received_buf[..bytes_received], data.as_slice());
  }
}

#[test]
fn test_send_recv_with_flags() {
  let mut lio = Lio::new(64).unwrap();

  let common::TcpPair { server_sock: _, client_sock, accepted_fd } =
    setup_tcp_pair(&mut lio);

  // Send with MSG_NOSIGNAL flag (prevents SIGPIPE)
  let data = b"Data with flags".to_vec();
  let data_len = data.len();

  let (sender_send, receiver_send) = mpsc::channel();
  api::send(&client_sock, data.clone(), Some(libc::MSG_NOSIGNAL))
    .with_lio(&mut lio)
    .send_with(sender_send);

  let (bytes_sent, _) = poll_until_recv(&mut lio, &receiver_send);
  let bytes_sent = bytes_sent.expect("Failed to send with flags") as usize;

  assert_eq!(bytes_sent, data_len);

  // Receive with flags (0 is valid)
  let (sender_recv, receiver_recv) = mpsc::channel();
  api::recv(&accepted_fd, vec![0u8; 64], Some(0))
    .with_lio(&mut lio)
    .send_with(sender_recv);

  let (bytes_received, received_buf) =
    poll_until_recv(&mut lio, &receiver_recv);
  let bytes_received =
    bytes_received.expect("Failed to receive with flags") as usize;

  assert_eq!(bytes_received, data_len);
  assert_eq!(&received_buf[..bytes_received], data.as_slice());
}

#[test]
fn test_recv_on_closed_connection() {
  let mut lio = Lio::new(64).unwrap();

  let common::TcpPair { server_sock: _, client_sock, accepted_fd } =
    setup_tcp_pair(&mut lio);

  // Close the client side
  drop(client_sock);

  // Try to receive - should return 0 (EOF)
  let (sender_recv, receiver_recv) = mpsc::channel();
  api::recv(&accepted_fd, vec![0u8; 64], None)
    .with_lio(&mut lio)
    .send_with(sender_recv);

  let (bytes_received, _) = poll_until_recv(&mut lio, &receiver_recv);
  let bytes_received = bytes_received.expect("recv should succeed with 0");

  assert_eq!(
    bytes_received, 0,
    "Should receive EOF (0 bytes) on closed connection"
  );
}

#[test]
fn test_recv_partial_buffer() {
  let mut lio = Lio::new(64).unwrap();

  let common::TcpPair { server_sock: _, client_sock, accepted_fd } =
    setup_tcp_pair(&mut lio);

  // Send more data than the receive buffer
  let data = b"This is a longer message that exceeds buffer".to_vec();

  let (sender_send, receiver_send) = mpsc::channel();
  api::send(&client_sock, data.clone(), None)
    .with_lio(&mut lio)
    .send_with(sender_send);

  poll_until_recv(&mut lio, &receiver_send).0.unwrap();

  // Receive with smaller buffer
  let (sender_recv, receiver_recv) = mpsc::channel();
  api::recv(&accepted_fd, vec![0u8; 10], None)
    .with_lio(&mut lio)
    .send_with(sender_recv);

  let (bytes_received, received_buf) =
    poll_until_recv(&mut lio, &receiver_recv);
  let bytes_received = bytes_received.expect("Failed to receive") as usize;

  // Should receive up to buffer size
  assert!(bytes_received <= 10);
  assert_eq!(&received_buf[..bytes_received], &data[..bytes_received]);
}

#[test]
fn test_send_recv_concurrent_pairs() {
  let mut lio = Lio::new(256).unwrap();

  // Create 5 concurrent client/server pairs
  let num_pairs = 5;
  let mut pairs = Vec::new();

  for _ in 0..num_pairs {
    let pair = setup_tcp_pair(&mut lio);
    pairs.push(pair);
  }

  // Send from all clients simultaneously
  let (sender_send, receiver_send) = mpsc::channel();
  for (i, pair) in pairs.iter().enumerate() {
    let data = format!("Client {} data", i).into_bytes();
    api::send(&pair.client_sock, data, None)
      .with_lio(&mut lio)
      .send_with(sender_send.clone());
  }

  // Collect all send results
  for _ in 0..num_pairs {
    let (res, _) = poll_until_recv(&mut lio, &receiver_send);
    res.expect("Send should succeed");
  }

  // Receive from all accepted connections
  let (sender_recv, receiver_recv) = mpsc::channel();
  for pair in &pairs {
    api::recv(&pair.accepted_fd, vec![0u8; 64], None)
      .with_lio(&mut lio)
      .send_with(sender_recv.clone());
  }

  // Collect all recv results
  for _ in 0..num_pairs {
    let (res, buf) = poll_until_recv(&mut lio, &receiver_recv);
    let bytes = res.expect("Recv should succeed") as usize;
    assert!(bytes > 0);
    // Verify it's one of our messages
    let msg = String::from_utf8_lossy(&buf[..bytes]);
    assert!(msg.starts_with("Client "));
  }
}
