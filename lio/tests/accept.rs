mod common;

use common::poll_until_recv;
use lio::Lio;
use lio::api;
use std::{
  mem::MaybeUninit,
  net::SocketAddr,
  os::fd::{AsFd, AsRawFd},
  sync::mpsc,
};

#[test]
fn test_accept_basic() {
  let mut lio = Lio::new(64).unwrap();

  let (sender_sock, receiver_sock) = mpsc::channel();

  // Create and setup server socket
  lio::test_utils::tcp_socket()
    .with_lio(&mut lio)
    .send_with(sender_sock.clone());

  let server_sock = poll_until_recv(&mut lio, &receiver_sock)
    .expect("socket syscall wasn't done");

  let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();

  let (sender_bind, receiver_bind) = mpsc::channel();
  api::bind(&server_sock, addr).with_lio(&mut lio).send_with(sender_bind);

  poll_until_recv(&mut lio, &receiver_bind).unwrap();

  let bound_addr = unsafe {
    let mut addr_storage = MaybeUninit::<libc::sockaddr_in>::zeroed();
    let mut addr_len =
      std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
    libc::getsockname(
      server_sock.as_fd().as_raw_fd(),
      addr_storage.as_mut_ptr() as *mut libc::sockaddr,
      &mut addr_len,
    );
    let sockaddr_in = addr_storage.assume_init();
    let port = u16::from_be(sockaddr_in.sin_port);
    format!("127.0.0.1:{}", port).parse::<SocketAddr>().unwrap()
  };

  let (sender_listen, receiver_listen) = mpsc::channel();
  api::listen(&server_sock, 128).with_lio(&mut lio).send_with(sender_listen);

  poll_until_recv(&mut lio, &receiver_listen).expect("listen syscall failed");

  lio::test_utils::tcp_socket()
    .with_lio(&mut lio)
    .send_with(sender_sock.clone());

  let client_sock = poll_until_recv(&mut lio, &receiver_sock)
    .expect("socket didn't finish after tick");

  let (sender_connect, receiver_connect) = mpsc::channel();
  let (sender_accept, receiver_accept) = mpsc::channel();

  api::connect(&client_sock, bound_addr)
    .with_lio(&mut lio)
    .send_with(sender_connect);
  api::accept(&server_sock).with_lio(&mut lio).send_with(sender_accept);

  let (accepted_fd, _) = poll_until_recv(&mut lio, &receiver_accept).unwrap();
  poll_until_recv(&mut lio, &receiver_connect).expect("Failed to connect");

  assert!(accepted_fd.as_fd().as_raw_fd() >= 0, "Accepted fd should be valid");

  let data = vec![1, 2, 3, 4];
  let (sender_send, receiver_send) = mpsc::channel();
  let (sender_recv, receiver_recv) = mpsc::channel();

  api::send(&client_sock, data.clone(), None)
    .with_lio(&mut lio)
    .send_with(sender_send);
  api::recv(&accepted_fd, Vec::with_capacity(4), None)
    .with_lio(&mut lio)
    .send_with(sender_recv);

  let (res, _buf) = poll_until_recv(&mut lio, &receiver_send);
  res.unwrap();

  let (res, buf) = poll_until_recv(&mut lio, &receiver_recv);
  res.unwrap();
  assert!(buf == _buf);
}

#[test]
fn test_accept_multiple() {
  let mut lio = Lio::new(64).unwrap();

  let (sender_sock, receiver_sock) = mpsc::channel();
  let (sender_unit, receiver_unit) = mpsc::channel();

  // Create server socket
  lio::test_utils::tcp_socket()
    .with_lio(&mut lio)
    .send_with(sender_sock.clone());

  let server_sock = poll_until_recv(&mut lio, &receiver_sock)
    .expect("Failed to create server socket");

  let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();

  api::bind(&server_sock, addr)
    .with_lio(&mut lio)
    .send_with(sender_unit.clone());

  poll_until_recv(&mut lio, &receiver_unit).expect("Failed to bind");

  let bound_addr = unsafe {
    let mut addr_storage = MaybeUninit::<libc::sockaddr_in>::zeroed();
    let mut addr_len =
      std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
    libc::getsockname(
      server_sock.as_fd().as_raw_fd(),
      addr_storage.as_mut_ptr() as *mut libc::sockaddr,
      &mut addr_len,
    );
    let sockaddr_in = addr_storage.assume_init();
    let port = u16::from_be(sockaddr_in.sin_port);
    format!("127.0.0.1:{}", port).parse::<SocketAddr>().unwrap()
  };

  api::listen(&server_sock, 128)
    .with_lio(&mut lio)
    .send_with(sender_unit.clone());

  poll_until_recv(&mut lio, &receiver_unit).expect("Failed to listen");

  let num_clients = 5;
  let mut accepted_fds = Vec::new();
  let mut client_fds = Vec::new();

  for _ in 0..num_clients {
    let (sender_a, receiver_a) = mpsc::channel();
    let (sender_s, receiver_s) = mpsc::channel();
    let (sender_c, receiver_c) = mpsc::channel();

    // Accept connection
    api::accept(&server_sock).with_lio(&mut lio).send_with(sender_a);

    // Create client socket
    lio::test_utils::tcp_socket().with_lio(&mut lio).send_with(sender_s);

    let client_sock = poll_until_recv(&mut lio, &receiver_s)
      .expect("Failed to create client socket");

    api::connect(&client_sock, bound_addr)
      .with_lio(&mut lio)
      .send_with(sender_c);

    poll_until_recv(&mut lio, &receiver_c).expect("Failed to connect");

    let (accepted_fd, _) =
      poll_until_recv(&mut lio, &receiver_a).expect("Failed to accept");

    accepted_fds.push(accepted_fd);
    client_fds.push(client_sock);
  }

  // Verify all connections
  assert_eq!(accepted_fds.len(), num_clients);
  assert_eq!(client_fds.len(), num_clients);
}

#[test]
fn test_accept_with_client_info() {
  let mut lio = Lio::new(64).unwrap();

  let (sender_sock, receiver_sock) = mpsc::channel();
  let (sender_unit, receiver_unit) = mpsc::channel();

  lio::test_utils::tcp_socket()
    .with_lio(&mut lio)
    .send_with(sender_sock.clone());

  let server_sock = poll_until_recv(&mut lio, &receiver_sock)
    .expect("Failed to create server socket");

  let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();

  api::bind(&server_sock, addr)
    .with_lio(&mut lio)
    .send_with(sender_unit.clone());

  poll_until_recv(&mut lio, &receiver_unit).expect("Failed to bind");

  let bound_addr = unsafe {
    let mut addr_storage = MaybeUninit::<libc::sockaddr_in>::zeroed();
    let mut addr_len =
      std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
    libc::getsockname(
      server_sock.as_fd().as_raw_fd(),
      addr_storage.as_mut_ptr() as *mut libc::sockaddr,
      &mut addr_len,
    );
    let sockaddr_in = addr_storage.assume_init();
    let port = u16::from_be(sockaddr_in.sin_port);
    format!("127.0.0.1:{}", port).parse::<SocketAddr>().unwrap()
  };

  api::listen(&server_sock, 128)
    .with_lio(&mut lio)
    .send_with(sender_unit.clone());

  poll_until_recv(&mut lio, &receiver_unit).expect("Failed to listen");

  let (sender_a, receiver_a) = mpsc::channel();
  let (sender_s, receiver_s) = mpsc::channel();
  let (sender_c, receiver_c) = mpsc::channel();

  api::accept(&server_sock).with_lio(&mut lio).send_with(sender_a);

  lio::test_utils::tcp_socket().with_lio(&mut lio).send_with(sender_s);

  let client_sock = poll_until_recv(&mut lio, &receiver_s)
    .expect("Failed to create client socket");

  api::connect(&client_sock, bound_addr).with_lio(&mut lio).send_with(sender_c);

  poll_until_recv(&mut lio, &receiver_c).expect("Failed to connect");

  let (accepted_fd, _client_addr) =
    poll_until_recv(&mut lio, &receiver_a).expect("Failed to accept");
  let _ = accepted_fd;
}

#[test]
fn test_accept_ipv6() {
  let mut lio = Lio::new(64).unwrap();

  let (sender_sock, receiver_sock) = mpsc::channel();
  let (sender_unit, receiver_unit) = mpsc::channel();

  lio::test_utils::tcp6_socket()
    .with_lio(&mut lio)
    .send_with(sender_sock.clone());

  let server_sock = poll_until_recv(&mut lio, &receiver_sock)
    .expect("Failed to create IPv6 server socket");

  let addr: SocketAddr = "[::1]:0".parse().unwrap();

  api::bind(&server_sock, addr)
    .with_lio(&mut lio)
    .send_with(sender_unit.clone());

  poll_until_recv(&mut lio, &receiver_unit).expect("Failed to bind IPv6");

  let bound_addr = unsafe {
    let mut addr_storage = MaybeUninit::<libc::sockaddr_in6>::zeroed();
    let mut addr_len =
      std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t;
    libc::getsockname(
      server_sock.as_fd().as_raw_fd(),
      addr_storage.as_mut_ptr() as *mut libc::sockaddr,
      &mut addr_len,
    );
    let sockaddr_in6 = addr_storage.assume_init();
    let port = u16::from_be(sockaddr_in6.sin6_port);
    format!("[::1]:{}", port).parse::<SocketAddr>().unwrap()
  };

  api::listen(&server_sock, 128)
    .with_lio(&mut lio)
    .send_with(sender_unit.clone());

  poll_until_recv(&mut lio, &receiver_unit).expect("Failed to listen");

  let (sender_a, receiver_a) = mpsc::channel();

  api::accept(&server_sock).with_lio(&mut lio).send_with(sender_a);

  lio::test_utils::tcp6_socket()
    .with_lio(&mut lio)
    .send_with(sender_sock.clone());

  let client_sock = poll_until_recv(&mut lio, &receiver_sock)
    .expect("Failed to create IPv6 client socket");

  let (sender_c, receiver_c) = mpsc::channel();
  api::connect(&client_sock, bound_addr).with_lio(&mut lio).send_with(sender_c);

  poll_until_recv(&mut lio, &receiver_c).expect("connect error");

  let (accepted_fd, _) =
    poll_until_recv(&mut lio, &receiver_a).expect("Failed to accept IPv6");

  assert!(accepted_fd.as_fd().as_raw_fd() >= 0);
}
