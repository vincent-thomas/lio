use lio::{accept, bind, connect, listen, socket};
use socket2::{Domain, Protocol, Type};
use std::{
  mem::MaybeUninit,
  net::SocketAddr,
  sync::mpsc::{self, TryRecvError},
};

#[test]
fn test_accept_basic() {
  lio::init();

  // Create and setup server socket
  let mut recv = socket(Domain::IPV4, Type::STREAM, None).send();

  lio::tick();

  let server_sock = recv
    .try_recv()
    .expect("socket syscall wasn't done")
    .expect("socket syscall failed");

  let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();

  let mut recv = bind(server_sock, addr).send();

  // assert!(recv.try_recv().is_none());
  lio::tick();

  recv.try_recv().unwrap().expect("Failed to bind");

  let bound_addr = unsafe {
    let mut addr_storage = MaybeUninit::<libc::sockaddr_in>::zeroed();
    let mut addr_len =
      std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
    libc::getsockname(
      server_sock,
      addr_storage.as_mut_ptr() as *mut libc::sockaddr,
      &mut addr_len,
    );
    let sockaddr_in = addr_storage.assume_init();
    let port = u16::from_be(sockaddr_in.sin_port);
    format!("127.0.0.1:{}", port).parse::<SocketAddr>().unwrap()
  };

  let mut recv = listen(server_sock, 128).send();

  // assert!(recv.try_recv().is_none());

  lio::tick();

  assert!(
    recv.try_recv().expect("listen didn't respond after tick").is_ok(),
    "listen syscall failed"
  );

  let mut recv = socket(Domain::IPV4, Type::STREAM, None).send();

  // assert!(recv.try_recv().is_none());

  lio::tick();

  let client_sock = recv
    .try_recv()
    .expect("socket didn't finish after tick")
    .expect("failed socket syscall");

  let mut connect_recv = connect(client_sock, bound_addr).send();
  let mut accept_recv = accept(server_sock).send();

  println!("nice");
  // assert!(connect_recv.try_recv().is_none());
  println!("nice");
  // assert!(accept_recv.try_recv().is_none());

  lio::tick();
  lio::tick();

  let (accepted_fd, addr) = accept_recv
    .try_recv()
    .unwrap()
    .expect("accept syscall didn't return after a tick");
  // .expect("accept syscall failed");

  assert!(accepted_fd >= 0, "Accepted fd should be valid");

  // Cleanup
  unsafe {
    libc::close(accepted_fd);
    libc::close(server_sock);
    libc::close(client_sock);
  }
}

#[test]
fn test_accept_multiple() {
  lio::init();

  let (sender_sock, receiver_sock) = mpsc::channel();
  let (sender_unit, receiver_unit) = mpsc::channel();

  // Create server socket
  socket(Domain::IPV4, Type::STREAM, Some(Protocol::TCP)).when_done(
    move |res| {
      sender_sock.send(res).unwrap();
    },
  );

  // assert_eq!(receiver_sock.try_recv().unwrap_err(), TryRecvError::Empty);
  lio::tick();

  let server_sock =
    receiver_sock.recv().unwrap().expect("Failed to create server socket");

  let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();

  let sender_b = sender_unit.clone();
  bind(server_sock, addr).when_done(move |res| {
    sender_b.send(res).unwrap();
  });

  // assert_eq!(receiver_unit.try_recv().unwrap_err(), TryRecvError::Empty);
  lio::tick();

  receiver_unit.recv().unwrap().expect("Failed to bind");

  let bound_addr = unsafe {
    let mut addr_storage = MaybeUninit::<libc::sockaddr_in>::zeroed();
    let mut addr_len =
      std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
    libc::getsockname(
      server_sock,
      addr_storage.as_mut_ptr() as *mut libc::sockaddr,
      &mut addr_len,
    );
    let sockaddr_in = addr_storage.assume_init();
    let port = u16::from_be(sockaddr_in.sin_port);
    format!("127.0.0.1:{}", port).parse::<SocketAddr>().unwrap()
  };

  let sender_l = sender_unit.clone();
  listen(server_sock, 128).when_done(move |res| {
    sender_l.send(res).unwrap();
  });

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
    accept(server_sock).when_done(move |res| {
      sender_a.send(res).unwrap();
    });

    // Create client socket
    socket(Domain::IPV4, Type::STREAM, Some(Protocol::TCP)).when_done(
      move |res| {
        sender_s.send(res).unwrap();
      },
    );

    // assert_eq!(receiver_a.try_recv().unwrap_err(), TryRecvError::Empty);
    lio::tick();

    let client_sock =
      receiver_s.recv().unwrap().expect("Failed to create client socket");

    connect(client_sock, bound_addr).when_done(move |res| {
      sender_c.send(res).unwrap();
    });

    // assert_eq!(receiver_c.try_recv().unwrap_err(), TryRecvError::Empty);
    lio::tick();

    receiver_c.recv().unwrap().expect("Failed to connect");

    lio::tick();

    let (accepted_fd, _) =
      receiver_a.recv().unwrap().expect("Failed to accept");

    accepted_fds.push(accepted_fd);
    client_fds.push(client_sock);
  }

  // Verify all connections
  assert_eq!(accepted_fds.len(), num_clients);
  assert_eq!(client_fds.len(), num_clients);

  // Cleanup
  unsafe {
    for fd in accepted_fds {
      libc::close(fd);
    }
    for fd in client_fds {
      libc::close(fd);
    }
    libc::close(server_sock);
  }
}

#[test]
fn test_accept_with_client_info() {
  lio::init();

  let (sender_sock, receiver_sock) = mpsc::channel();
  let (sender_unit, receiver_unit) = mpsc::channel();

  socket(Domain::IPV4, Type::STREAM, Some(Protocol::TCP)).when_done(
    move |res| {
      sender_sock.send(res).unwrap();
    },
  );

  // assert_eq!(receiver_sock.try_recv().unwrap_err(), TryRecvError::Empty);
  lio::tick();

  let server_sock =
    receiver_sock.recv().unwrap().expect("Failed to create server socket");

  let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();

  let sender_b = sender_unit.clone();
  bind(server_sock, addr).when_done(move |res| {
    sender_b.send(res).unwrap();
  });

  // assert_eq!(receiver_unit.try_recv().unwrap_err(), TryRecvError::Empty);
  lio::tick();

  receiver_unit.recv().unwrap().expect("Failed to bind");

  let bound_addr = unsafe {
    let mut addr_storage = MaybeUninit::<libc::sockaddr_in>::zeroed();
    let mut addr_len =
      std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
    libc::getsockname(
      server_sock,
      addr_storage.as_mut_ptr() as *mut libc::sockaddr,
      &mut addr_len,
    );
    let sockaddr_in = addr_storage.assume_init();
    let port = u16::from_be(sockaddr_in.sin_port);
    format!("127.0.0.1:{}", port).parse::<SocketAddr>().unwrap()
  };

  let sender_l = sender_unit.clone();
  listen(server_sock, 128).when_done(move |res| {
    sender_l.send(res).unwrap();
  });

  // assert_eq!(receiver_unit.try_recv().unwrap_err(), TryRecvError::Empty);
  lio::tick();

  receiver_unit.recv().unwrap().expect("Failed to listen");

  let (sender_a, receiver_a) = mpsc::channel();
  let (sender_s, receiver_s) = mpsc::channel();
  let (sender_c, receiver_c) = mpsc::channel();

  accept(server_sock).when_done(move |res| {
    sender_a.send(res).unwrap();
  });

  socket(Domain::IPV4, Type::STREAM, Some(Protocol::TCP)).when_done(
    move |res| {
      sender_s.send(res).unwrap();
    },
  );

  // assert_eq!(receiver_a.try_recv().unwrap_err(), TryRecvError::Empty);
  lio::tick();

  let client_sock =
    receiver_s.recv().unwrap().expect("Failed to create client socket");

  connect(client_sock, bound_addr).when_done(move |res| {
    sender_c.send(res).unwrap();
  });

  // assert_eq!(receiver_c.try_recv().unwrap_err(), TryRecvError::Empty);
  lio::tick();

  receiver_c.recv().unwrap().expect("Failed to connect");

  lio::tick();

  let (accepted_fd, _client_addr) =
    receiver_a.recv().unwrap().expect("Failed to accept");

  // Cleanup
  unsafe {
    libc::close(client_sock);
    libc::close(accepted_fd);
    libc::close(server_sock);
  }
}

#[test]
fn test_accept_ipv6() {
  lio::init();

  // let (sender_sock, receiver_sock) = mpsc::channel();
  // let (sender_unit, receiver_unit) = mpsc::channel();

  let mut socket_recv =
    socket(Domain::IPV6, Type::STREAM, Some(Protocol::TCP)).send();

  // assert_eq!(receiver_sock.try_recv().unwrap_err(), TryRecvError::Empty);
  lio::tick();

  let server_sock = socket_recv
    .try_recv()
    .expect("socket didn't finish")
    .expect("Failed to create IPv6 server socket");

  let addr: SocketAddr = "[::1]:0".parse().unwrap();

  // let sender_b = sender_unit.clone();
  let mut bind_recv = bind(server_sock, addr).send();

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
      server_sock,
      addr_storage.as_mut_ptr() as *mut libc::sockaddr,
      &mut addr_len,
    );
    let sockaddr_in6 = addr_storage.assume_init();
    let port = u16::from_be(sockaddr_in6.sin6_port);
    format!("[::1]:{}", port).parse::<SocketAddr>().unwrap()
  };

  let mut listen_recv = listen(server_sock, 128).send();

  // assert_eq!(receiver_unit.try_recv().unwrap_err(), TryRecvError::Empty);
  lio::tick();

  listen_recv
    .try_recv()
    .expect("listen didn't complete")
    .expect("Failed to listen");

  let mut accept_recv = accept(server_sock).send();

  let mut client_s_recv =
    socket(Domain::IPV6, Type::STREAM, Some(Protocol::TCP)).send();

  // assert_eq!(receiver_s.try_recv().unwrap_err(), TryRecvError::Empty);
  lio::tick();

  let client_sock = client_s_recv
    .try_recv()
    .unwrap()
    .expect("Failed to create IPv6 client socket");

  let mut connect_recv = connect(client_sock, bound_addr).send();

  // assert_eq!(receiver_c.try_recv().unwrap_err(), TryRecvError::Empty);
  lio::tick();

  connect_recv
    .try_recv()
    .expect("connect didn't finish")
    .expect("connect error");

  lio::tick();

  let (accepted_fd, _) = accept_recv
    .try_recv()
    .expect("accept didn't finish")
    .expect("Failed to accept IPv6");

  assert!(accepted_fd >= 0);

  // Cleanup
  unsafe {
    libc::close(client_sock);
    libc::close(accepted_fd);
    libc::close(server_sock);
  }
}

#[test]
#[ignore = "See https://github.com/liten-rs/liten/issues/30"]
fn test_accept_concurrent() {
  lio::init();

  let mut socket_recv =
    socket(Domain::IPV4, Type::STREAM, Some(Protocol::TCP)).send();

  // assert_eq!(receiver_sock.try_recv().unwrap_err(), TryRecvError::Empty);
  lio::tick();

  let server_sock =
    socket_recv.try_recv().unwrap().expect("Failed to create server socket");

  let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();

  let mut bind_recv = bind(server_sock, addr).send();

  // assert_eq!(bind_recv.try_recv().unwrap_err(), TryRecvError::Empty);
  lio::tick();

  bind_recv.try_recv().unwrap().expect("failed to bind");

  let bound_addr = unsafe {
    let mut addr_storage = MaybeUninit::<libc::sockaddr_in>::zeroed();
    let mut addr_len =
      std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
    libc::getsockname(
      server_sock,
      addr_storage.as_mut_ptr() as *mut libc::sockaddr,
      &mut addr_len,
    );
    let sockaddr_in = addr_storage.assume_init();
    let port = u16::from_be(sockaddr_in.sin_port);
    format!("127.0.0.1:{}", port).parse::<SocketAddr>().unwrap()
  };

  let mut listen_recv = listen(server_sock, 128).send();

  // assert_eq!(listen_recv.try_recv().unwrap_err(), TryRecvError::Empty);
  lio::tick();

  listen_recv.try_recv().unwrap().expect("Failed to listen");

  let mut accept_queue = Vec::new();
  // Queue up all accepts
  for _ in 0..3 {
    accept_queue.push(accept(server_sock).send());
  }

  let mut all_sockets = Vec::new();

  // Queue up all socket creations
  for _ in 0..3 {
    all_sockets
      .push(socket(Domain::IPV4, Type::STREAM, Some(Protocol::TCP)).send());
  }

  lio::tick();

  // Collect all sockets
  let mut client_fds = Vec::new();
  for mut fd_recv in all_sockets {
    let client_sock =
      fd_recv.try_recv().unwrap().expect("Failed to create client socket");
    client_fds.push(client_sock);
  }

  let mut connect_recvs = Vec::new();

  // Connect all clients
  for &client_sock in &client_fds {
    let connect_fd = connect(client_sock, bound_addr).send();
    connect_recvs.push(connect_fd);
  }

  lio::tick();

  // Collect results - 3 connects
  for mut connect_recv in connect_recvs {
    connect_recv.try_recv().unwrap().expect("Failed to connect");
  }

  // Collect accepts
  let mut accepted_fds = Vec::new();
  for mut accept_recv in accept_queue {
    lio::tick();
    let (accepted_fd, _) =
      accept_recv.try_recv().unwrap().expect("Failed to accept");
    accepted_fds.push(accepted_fd);
  }

  assert_eq!(accepted_fds.len(), 3);

  // Cleanup
  unsafe {
    for fd in accepted_fds {
      libc::close(fd);
    }
    for fd in client_fds {
      libc::close(fd);
    }
    libc::close(server_sock);
  }
}
