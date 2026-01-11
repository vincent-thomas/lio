use lio::api;
use std::{
  mem::MaybeUninit,
  net::SocketAddr,
  os::fd::{AsFd, AsRawFd},
  sync::mpsc,
};

#[test]
fn test_accept_basic() {
  lio::init();

  // Create and setup server socket
  let recv = lio::test_utils::tcp_socket().send();

  lio::tick();

  let server_sock = recv.recv().expect("socket syscall wasn't done");

  let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();

  let recv = api::bind(&server_sock, addr).send();

  // assert!(recv.try_recv().is_none());
  lio::tick();

  recv.recv().unwrap();

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

  let recv = api::listen(&server_sock, 128).send();

  // assert!(recv.try_recv().is_none());

  lio::tick();

  assert!(recv.recv().is_ok(), "listen syscall failed");

  let recv = lio::test_utils::tcp_socket().send();

  // assert!(recv.try_recv().is_none());

  lio::tick();

  let client_sock = recv.recv().expect("socket didn't finish after tick");

  let connect_recv = api::connect(&client_sock, bound_addr).send();
  let accept_recv = api::accept(&server_sock).send();

  lio::tick();

  let (accepted_fd, _) = accept_recv.recv().unwrap();
  connect_recv.recv().expect("Failed to connect");

  assert!(accepted_fd.as_fd().as_raw_fd() >= 0, "Accepted fd should be valid");

  dbg!(client_sock.as_fd().as_raw_fd(), server_sock.as_raw_fd().as_raw_fd());

  let data = vec![1, 2, 3, 4];
  let send_recv = api::send(&client_sock, data.clone(), None).send();
  let recv_recv = api::recv(&accepted_fd, Vec::with_capacity(4), None).send();

  let (res, _buf) = send_recv.recv();
  res.unwrap();

  let (res, buf) = recv_recv.recv();
  res.unwrap();
  assert!(buf == _buf);

  // Cleanup
  unsafe {
    libc::close(accepted_fd.as_fd().as_raw_fd());
    libc::close(server_sock.as_fd().as_raw_fd());
    libc::close(client_sock.as_fd().as_raw_fd());
  }
  lio::exit();
}

#[test]
fn test_accept_multiple() {
  lio::init();

  let (sender_sock, receiver_sock) = mpsc::channel();
  let (sender_unit, receiver_unit) = mpsc::channel();

  // Create server socket
  lio::test_utils::tcp_socket().send_with(sender_sock);

  // assert_eq!(receiver_sock.try_recv().unwrap_err(), TryRecvError::Empty);
  lio::tick();

  let server_sock =
    receiver_sock.recv().unwrap().expect("Failed to create server socket");

  let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();

  api::bind(&server_sock, addr).send_with(sender_unit.clone());

  // assert_eq!(receiver_unit.try_recv().unwrap_err(), TryRecvError::Empty);
  lio::tick();

  receiver_unit.recv().unwrap().expect("Failed to bind");

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

  api::listen(&server_sock, 128).send_with(sender_unit.clone());

  // assert_eq!(receiver_unit.try_recv().unwrap_err(), TryRecvError::Empty);
  lio::tick();

  receiver_unit.recv().unwrap().expect("Failed to listen");

  let num_clients = 5;
  let mut accepted_fds = Vec::new();
  let mut client_fds = Vec::new();

  for _ in 0..num_clients {
    let (sender_a, receiver_a) = mpsc::channel();
    let (sender_s, receiver_s) = mpsc::channel();
    let (sender_c, receiver_c) = mpsc::channel();

    // Accept connection
    api::accept(&server_sock).send_with(sender_a);

    // Create client socket
    lio::test_utils::tcp_socket().send_with(sender_s);

    // assert_eq!(receiver_a.try_recv().unwrap_err(), TryRecvError::Empty);
    lio::tick();

    let client_sock =
      receiver_s.recv().unwrap().expect("Failed to create client socket");

    api::connect(&client_sock, bound_addr).send_with(sender_c);

    // assert_eq!(receiver_c.try_recv().unwrap_err(), TryRecvError::Empty);
    // Multiple ticks for connect/accept to complete
    for _ in 0..10 {
      lio::tick();
    }

    receiver_c
      .recv()
      .expect("connect channel disconnected")
      .expect("Failed to connect");

    // Additional ticks for accept
    for _ in 0..10 {
      lio::tick();
    }

    let (accepted_fd, _) = receiver_a
      .recv()
      .expect("accept channel disconnected")
      .expect("Failed to accept");

    accepted_fds.push(accepted_fd);
    client_fds.push(client_sock);
  }

  // Verify all connections
  assert_eq!(accepted_fds.len(), num_clients);
  assert_eq!(client_fds.len(), num_clients);

  // Cleanup
  unsafe {
    for fd in accepted_fds {
      libc::close(fd.as_fd().as_raw_fd());
    }
    for fd in client_fds {
      libc::close(fd.as_fd().as_raw_fd());
    }
    libc::close(server_sock.as_fd().as_raw_fd());
  }
}

#[test]
fn test_accept_with_client_info() {
  lio::init();

  let (sender_sock, receiver_sock) = mpsc::channel();
  let (sender_unit, receiver_unit) = mpsc::channel();

  lio::test_utils::tcp_socket().send_with(sender_sock);

  // assert_eq!(receiver_sock.try_recv().unwrap_err(), TryRecvError::Empty);
  lio::tick();

  let server_sock =
    receiver_sock.recv().unwrap().expect("Failed to create server socket");

  let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();

  api::bind(&server_sock, addr).send_with(sender_unit.clone());

  // assert_eq!(receiver_unit.try_recv().unwrap_err(), TryRecvError::Empty);
  lio::tick();

  receiver_unit.recv().unwrap().expect("Failed to bind");

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

  api::listen(&server_sock, 128).send_with(sender_unit.clone());

  // assert_eq!(receiver_unit.try_recv().unwrap_err(), TryRecvError::Empty);
  lio::tick();

  receiver_unit.recv().unwrap().expect("Failed to listen");

  let (sender_a, receiver_a) = mpsc::channel();
  let (sender_s, receiver_s) = mpsc::channel();
  let (sender_c, receiver_c) = mpsc::channel();

  api::accept(&server_sock).send_with(sender_a);

  lio::test_utils::tcp_socket().send_with(sender_s);

  // assert_eq!(receiver_a.try_recv().unwrap_err(), TryRecvError::Empty);
  lio::tick();

  let client_sock =
    receiver_s.recv().unwrap().expect("Failed to create client socket");

  api::connect(&client_sock, bound_addr).send_with(sender_c);

  // assert_eq!(receiver_c.try_recv().unwrap_err(), TryRecvError::Empty);
  // Multiple ticks for connect/accept to complete
  for _ in 0..10 {
    lio::tick();
  }

  receiver_c
    .recv()
    .expect("connect channel disconnected")
    .expect("Failed to connect");

  // Additional ticks for accept
  for _ in 0..10 {
    lio::tick();
  }

  let (accepted_fd, _client_addr) = receiver_a
    .recv()
    .expect("accept channel disconnected")
    .expect("Failed to accept");

  // Cleanup
  unsafe {
    libc::close(client_sock.as_fd().as_raw_fd());
    libc::close(accepted_fd.as_fd().as_raw_fd());
    libc::close(server_sock.as_fd().as_raw_fd());
  }
}

#[test]
fn test_accept_ipv6() {
  lio::init();

  // let (sender_sock, receiver_sock) = mpsc::channel();
  // let (sender_unit, receiver_unit) = mpsc::channel();

  let mut socket_recv = lio::test_utils::tcp6_socket().send();

  // assert_eq!(receiver_sock.try_recv().unwrap_err(), TryRecvError::Empty);
  lio::tick();

  let server_sock = socket_recv
    .try_recv()
    .expect("socket didn't finish")
    .expect("Failed to create IPv6 server socket");

  let addr: SocketAddr = "[::1]:0".parse().unwrap();

  // let sender_b = sender_unit.clone();
  let mut bind_recv = api::bind(&server_sock, addr).send();

  // assert_eq!(receiver_unit.try_recv().unwrap_err(), TryRecvError::Empty);
  lio::tick();

  bind_recv
    .try_recv()
    .expect("bind didn't finish")
    .expect("Failed to bind IPv6");

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

  let listen_recv = api::listen(&server_sock, 128).send();

  // assert_eq!(receiver_unit.try_recv().unwrap_err(), TryRecvError::Empty);
  lio::tick();

  listen_recv.recv().expect("Failed to listen");

  let accept_recv = api::accept(&server_sock).send();

  let client_s_recv = lio::test_utils::tcp6_socket().send();

  // assert_eq!(receiver_s.try_recv().unwrap_err(), TryRecvError::Empty);
  lio::tick();

  let client_sock =
    client_s_recv.recv().expect("Failed to create IPv6 client socket");

  let connect_recv = api::connect(&client_sock, bound_addr).send();

  // assert_eq!(receiver_c.try_recv().unwrap_err(), TryRecvError::Empty);
  lio::tick();
  lio::tick(); // Connect/accept need multiple ticks

  connect_recv.recv().expect("connect error");

  lio::tick();

  let (accepted_fd, _) = accept_recv.recv().expect("Failed to accept IPv6");

  assert!(accepted_fd.as_fd().as_raw_fd() >= 0);

  // Cleanup
  unsafe {
    libc::close(client_sock.as_fd().as_raw_fd());
    libc::close(accepted_fd.as_fd().as_raw_fd());
    libc::close(server_sock.as_fd().as_raw_fd());
  }
}
