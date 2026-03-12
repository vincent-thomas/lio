//! Tests for Unix domain socket operations.
//!
//! Tests socket/bind/listen/connect/accept/send/recv for Unix sockets.
//!
//! NOTE: These tests are Linux-only because the accept() API returns a SocketAddr
//! which doesn't work with Unix domain sockets on other platforms.

#![cfg(target_os = "linux")]

mod common;

use common::{poll_recv, poll_until_recv};
use lio::api::resource::Resource;
use lio::{Lio, api};
use std::ffi::CString;
use std::os::fd::{AsFd, AsRawFd, FromRawFd};
use std::sync::mpsc;

/// RAII wrapper for Unix socket path cleanup
struct UnixSocketPath {
  path: CString,
}

impl UnixSocketPath {
  fn new(name: &str) -> Self {
    let path = CString::new(format!(
      "/tmp/lio_test_unix_{}_{}.sock",
      name,
      std::process::id()
    ))
    .expect("Failed to create CString path");
    // Remove if exists from previous run
    unsafe {
      libc::unlink(path.as_ptr());
    }
    Self { path }
  }
}

impl Drop for UnixSocketPath {
  fn drop(&mut self) {
    unsafe {
      libc::unlink(self.path.as_ptr());
    }
  }
}

fn bind_unix_socket(sock: &Resource, path: &CString) -> std::io::Result<()> {
  let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
  addr.sun_family = libc::AF_UNIX as _;

  let path_bytes = path.as_bytes();
  if path_bytes.len() >= addr.sun_path.len() {
    return Err(std::io::Error::new(
      std::io::ErrorKind::InvalidInput,
      "Path too long",
    ));
  }

  unsafe {
    std::ptr::copy_nonoverlapping(
      path_bytes.as_ptr(),
      addr.sun_path.as_mut_ptr() as *mut u8,
      path_bytes.len(),
    );
  }

  let result = unsafe {
    libc::bind(
      sock.as_fd().as_raw_fd(),
      &addr as *const _ as *const libc::sockaddr,
      std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t,
    )
  };

  if result < 0 { Err(std::io::Error::last_os_error()) } else { Ok(()) }
}

fn connect_unix_socket(sock: &Resource, path: &CString) -> std::io::Result<()> {
  let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
  addr.sun_family = libc::AF_UNIX as _;

  let path_bytes = path.as_bytes();
  unsafe {
    std::ptr::copy_nonoverlapping(
      path_bytes.as_ptr(),
      addr.sun_path.as_mut_ptr() as *mut u8,
      path_bytes.len(),
    );
  }

  let result = unsafe {
    libc::connect(
      sock.as_fd().as_raw_fd(),
      &addr as *const _ as *const libc::sockaddr,
      std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t,
    )
  };

  if result < 0 { Err(std::io::Error::last_os_error()) } else { Ok(()) }
}

#[test]
fn test_unix_stream_basic() {
  let mut lio = Lio::new(64).unwrap();
  let sock_path = UnixSocketPath::new("stream_basic");

  // Create server socket
  let mut server_recv =
    lio::test_utils::unix_stream_socket().with_lio(&mut lio).send();

  let server_sock = poll_recv(&mut lio, &mut server_recv)
    .expect("Failed to create server socket");

  // Bind to path
  bind_unix_socket(&server_sock, &sock_path.path).expect("Failed to bind");

  // Listen
  let (sender, receiver) = mpsc::channel();
  api::listen(&server_sock, 128).with_lio(&mut lio).send_with(sender.clone());
  poll_until_recv(&mut lio, &receiver).expect("Failed to listen");

  // Create client socket
  let mut client_recv =
    lio::test_utils::unix_stream_socket().with_lio(&mut lio).send();

  let client_sock = poll_recv(&mut lio, &mut client_recv)
    .expect("Failed to create client socket");

  // Set client to non-blocking for connect
  unsafe {
    let flags = libc::fcntl(client_sock.as_fd().as_raw_fd(), libc::F_GETFL, 0);
    libc::fcntl(
      client_sock.as_fd().as_raw_fd(),
      libc::F_SETFL,
      flags | libc::O_NONBLOCK,
    );
  }

  // Start accept
  let (sender_accept, receiver_accept) = mpsc::channel();
  api::accept_unix(&server_sock).with_lio(&mut lio).send_with(sender_accept);

  // Connect (synchronous for Unix sockets since they're local)
  connect_unix_socket(&client_sock, &sock_path.path)
    .expect("Failed to connect");

  // Wait for accept
  let accepted_fd =
    poll_until_recv(&mut lio, &receiver_accept).expect("Failed to accept");

  // Send data from client to server
  let data = b"Hello, Unix socket!".to_vec();
  let (sender_send, receiver_send) = mpsc::channel();
  api::send(&client_sock, data.clone(), None)
    .with_lio(&mut lio)
    .send_with(sender_send);

  let (bytes_sent, _) = poll_until_recv(&mut lio, &receiver_send);
  let bytes_sent = bytes_sent.expect("Failed to send") as usize;
  assert_eq!(bytes_sent, data.len());

  // Receive on server
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

#[test]
fn test_unix_stream_bidirectional() {
  let mut lio = Lio::new(64).unwrap();
  let sock_path = UnixSocketPath::new("stream_bidir");

  // Setup server
  let mut server_recv =
    lio::test_utils::unix_stream_socket().with_lio(&mut lio).send();
  let server_sock = poll_recv(&mut lio, &mut server_recv)
    .expect("Failed to create server socket");
  bind_unix_socket(&server_sock, &sock_path.path).expect("Failed to bind");

  let (sender, receiver) = mpsc::channel();
  api::listen(&server_sock, 128).with_lio(&mut lio).send_with(sender.clone());
  poll_until_recv(&mut lio, &receiver).expect("Failed to listen");

  // Setup client
  let mut client_recv =
    lio::test_utils::unix_stream_socket().with_lio(&mut lio).send();
  let client_sock = poll_recv(&mut lio, &mut client_recv)
    .expect("Failed to create client socket");

  // Accept and connect
  let (sender_accept, receiver_accept) = mpsc::channel();
  api::accept_unix(&server_sock).with_lio(&mut lio).send_with(sender_accept);
  connect_unix_socket(&client_sock, &sock_path.path)
    .expect("Failed to connect");
  let accepted_fd =
    poll_until_recv(&mut lio, &receiver_accept).expect("Failed to accept");

  // Send from client to server
  let client_msg = b"From client".to_vec();
  let (sender_send, receiver_send) = mpsc::channel();
  api::send(&client_sock, client_msg.clone(), None)
    .with_lio(&mut lio)
    .send_with(sender_send);
  poll_until_recv(&mut lio, &receiver_send).0.expect("Client send failed");

  // Receive on server
  let (sender_recv, receiver_recv) = mpsc::channel();
  api::recv(&accepted_fd, vec![0u8; 64], None)
    .with_lio(&mut lio)
    .send_with(sender_recv);
  let (bytes, buf) = poll_until_recv(&mut lio, &receiver_recv);
  let bytes = bytes.expect("Server recv failed") as usize;
  assert_eq!(&buf[..bytes], client_msg.as_slice());

  // Send from server to client
  let server_msg = b"From server".to_vec();
  let (sender_send2, receiver_send2) = mpsc::channel();
  api::send(&accepted_fd, server_msg.clone(), None)
    .with_lio(&mut lio)
    .send_with(sender_send2);
  poll_until_recv(&mut lio, &receiver_send2).0.expect("Server send failed");

  // Receive on client
  let (sender_recv2, receiver_recv2) = mpsc::channel();
  api::recv(&client_sock, vec![0u8; 64], None)
    .with_lio(&mut lio)
    .send_with(sender_recv2);
  let (bytes, buf) = poll_until_recv(&mut lio, &receiver_recv2);
  let bytes = bytes.expect("Client recv failed") as usize;
  assert_eq!(&buf[..bytes], server_msg.as_slice());
}

#[test]
fn test_unix_stream_multiple_clients() {
  let mut lio = Lio::new(64).unwrap();
  let sock_path = UnixSocketPath::new("stream_multi");

  // Setup server
  let mut server_recv =
    lio::test_utils::unix_stream_socket().with_lio(&mut lio).send();
  let server_sock = poll_recv(&mut lio, &mut server_recv)
    .expect("Failed to create server socket");
  bind_unix_socket(&server_sock, &sock_path.path).expect("Failed to bind");

  let (sender, receiver) = mpsc::channel();
  api::listen(&server_sock, 128).with_lio(&mut lio).send_with(sender);
  poll_until_recv(&mut lio, &receiver).expect("Failed to listen");

  let num_clients = 5;
  let mut clients = Vec::new();
  let mut accepted = Vec::new();

  for i in 0..num_clients {
    // Create client
    let mut client_recv =
      lio::test_utils::unix_stream_socket().with_lio(&mut lio).send();
    let client_sock = poll_recv(&mut lio, &mut client_recv)
      .expect(&format!("Failed to create client {}", i));

    // Accept
    let (sender_accept, receiver_accept) = mpsc::channel();
    api::accept_unix(&server_sock).with_lio(&mut lio).send_with(sender_accept);

    // Connect
    connect_unix_socket(&client_sock, &sock_path.path)
      .expect(&format!("Failed to connect client {}", i));

    let accepted_fd = poll_until_recv(&mut lio, &receiver_accept)
      .expect(&format!("Failed to accept client {}", i));

    clients.push(client_sock);
    accepted.push(accepted_fd);
  }

  assert_eq!(clients.len(), num_clients);
  assert_eq!(accepted.len(), num_clients);

  // Send unique message from each client
  let (sender_send, receiver_send) = mpsc::channel();
  for (i, client) in clients.iter().enumerate() {
    let msg = format!("Client {} message", i).into_bytes();
    api::send(client, msg, None)
      .with_lio(&mut lio)
      .send_with(sender_send.clone());
  }

  // Receive all messages
  for _ in 0..num_clients {
    poll_until_recv(&mut lio, &receiver_send).0.expect("Send failed");
  }

  // Verify each accepted connection can receive
  let (sender_recv, receiver_recv) = mpsc::channel();
  for accepted_fd in &accepted {
    api::recv(accepted_fd, vec![0u8; 64], None)
      .with_lio(&mut lio)
      .send_with(sender_recv.clone());
  }

  for i in 0..num_clients {
    let (bytes, buf) = poll_until_recv(&mut lio, &receiver_recv);
    let bytes = bytes.expect(&format!("Recv {} failed", i)) as usize;
    assert!(bytes > 0, "Should receive data from client {}", i);
    let msg = String::from_utf8_lossy(&buf[..bytes]);
    assert!(
      msg.starts_with("Client "),
      "Message should be from a client: {}",
      msg
    );
  }
}

#[test]
fn test_unix_dgram_basic() {
  let mut lio = Lio::new(64).unwrap();
  let server_path = UnixSocketPath::new("dgram_server");
  let client_path = UnixSocketPath::new("dgram_client");

  // Create server datagram socket
  let mut server_recv =
    lio::test_utils::unix_dgram_socket().with_lio(&mut lio).send();
  let server_sock = poll_recv(&mut lio, &mut server_recv)
    .expect("Failed to create server socket");

  // Bind server
  bind_unix_socket(&server_sock, &server_path.path)
    .expect("Failed to bind server");

  // Create client datagram socket
  let mut client_recv =
    lio::test_utils::unix_dgram_socket().with_lio(&mut lio).send();
  let client_sock = poll_recv(&mut lio, &mut client_recv)
    .expect("Failed to create client socket");

  // Bind client (needed for receiving responses)
  bind_unix_socket(&client_sock, &client_path.path)
    .expect("Failed to bind client");

  // Connect client to server (sets default destination)
  connect_unix_socket(&client_sock, &server_path.path)
    .expect("Failed to connect");

  // Send from client to server
  let data = b"Datagram message".to_vec();
  let (sender_send, receiver_send) = mpsc::channel();
  api::send(&client_sock, data.clone(), None)
    .with_lio(&mut lio)
    .send_with(sender_send);

  let (bytes_sent, _) = poll_until_recv(&mut lio, &receiver_send);
  let bytes_sent = bytes_sent.expect("Failed to send") as usize;
  assert_eq!(bytes_sent, data.len());

  // Receive on server
  let (sender_recv, receiver_recv) = mpsc::channel();
  api::recv(&server_sock, vec![0u8; 64], None)
    .with_lio(&mut lio)
    .send_with(sender_recv);

  let (bytes_received, received_buf) =
    poll_until_recv(&mut lio, &receiver_recv);
  let bytes_received = bytes_received.expect("Failed to receive") as usize;
  assert_eq!(bytes_received, data.len());
  assert_eq!(&received_buf[..bytes_received], data.as_slice());
}

#[test]
fn test_unix_shutdown() {
  let mut lio = Lio::new(64).unwrap();
  let sock_path = UnixSocketPath::new("shutdown");

  // Setup server
  let mut server_recv =
    lio::test_utils::unix_stream_socket().with_lio(&mut lio).send();
  let server_sock = poll_recv(&mut lio, &mut server_recv)
    .expect("Failed to create server socket");
  bind_unix_socket(&server_sock, &sock_path.path).expect("Failed to bind");

  let (sender, receiver) = mpsc::channel();
  api::listen(&server_sock, 128).with_lio(&mut lio).send_with(sender.clone());
  poll_until_recv(&mut lio, &receiver).expect("Failed to listen");

  // Setup client
  let mut client_recv =
    lio::test_utils::unix_stream_socket().with_lio(&mut lio).send();
  let client_sock = poll_recv(&mut lio, &mut client_recv)
    .expect("Failed to create client socket");

  // Accept and connect
  let (sender_accept, receiver_accept) = mpsc::channel();
  api::accept_unix(&server_sock).with_lio(&mut lio).send_with(sender_accept);
  connect_unix_socket(&client_sock, &sock_path.path)
    .expect("Failed to connect");
  let accepted_fd =
    poll_until_recv(&mut lio, &receiver_accept).expect("Failed to accept");

  // Shutdown write on client
  let mut shutdown_recv =
    api::shutdown(&client_sock, libc::SHUT_WR).with_lio(&mut lio).send();
  poll_recv(&mut lio, &mut shutdown_recv).expect("Shutdown failed");

  // Server should receive EOF
  let (sender_recv, receiver_recv) = mpsc::channel();
  api::recv(&accepted_fd, vec![0u8; 64], None)
    .with_lio(&mut lio)
    .send_with(sender_recv);

  let (bytes, _) = poll_until_recv(&mut lio, &receiver_recv);
  let bytes = bytes.expect("Recv should succeed") as usize;
  assert_eq!(bytes, 0, "Should receive EOF after shutdown");
}
