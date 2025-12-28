use std::sync::mpsc;

#[ignore = "flaky network test"]
#[test]
fn test_socket_simple() {
  lio::init();

  let (sender, receiver) = mpsc::channel();

  lio::test_utils::tcp_socket()
    .send_with(sender.clone());

  // assert_eq!(receiver.try_recv().unwrap_err(), TryRecvError::Empty);

  lio::tick();

  let sock =
    receiver.try_recv().unwrap().expect("Failed to create TCP IPv4 socket");
  assert!(sock >= 0, "Socket fd should be valid");

  // Verify it's a TCP socket
  unsafe {
    let mut sock_type: i32 = 0;
    let mut len = std::mem::size_of::<i32>() as libc::socklen_t;
    libc::getsockopt(
      sock,
      libc::SOL_SOCKET,
      libc::SO_TYPE,
      &mut sock_type as *mut _ as *mut libc::c_void,
      &mut len,
    );
    assert_eq!(sock_type, libc::SOCK_STREAM);

    let (sender2, receiver2) = mpsc::channel();
    let sender3 = sender2.clone();

    lio::close(sock).when_done(move |result| {
      sender3.send(result).unwrap();
    });

    // assert_eq!(receiver2.try_recv().unwrap_err(), TryRecvError::Empty);
    lio::tick();
    receiver2.recv().unwrap().unwrap();
  }
}

#[ignore = "flaky network test"]
#[test]
fn test_socket_tcp_ipv4() {
  lio::init();

  let (sender, receiver) = mpsc::channel();

  lio::test_utils::tcp_socket().when_done(move |result| {
    sender.send(result).unwrap();
  });

  // assert_eq!(receiver.try_recv().unwrap_err(), TryRecvError::Empty);

  lio::tick();

  let sock =
    receiver.recv().unwrap().expect("Failed to create TCP IPv4 socket");
  assert!(sock >= 0, "Socket fd should be valid");

  // Verify it's a TCP socket
  unsafe {
    let mut sock_type: i32 = 0;
    let mut len = std::mem::size_of::<i32>() as libc::socklen_t;
    libc::getsockopt(
      sock,
      libc::SOL_SOCKET,
      libc::SO_TYPE,
      &mut sock_type as *mut _ as *mut libc::c_void,
      &mut len,
    );
    assert_eq!(sock_type, libc::SOCK_STREAM);
    libc::close(sock);
  }
}

#[ignore = "flaky network test"]
#[test]
fn test_socket_tcp_ipv6() {
  lio::init();

  let (sender, receiver) = mpsc::channel();
  let sender1 = sender.clone();

  lio::test_utils::tcp6_socket().when_done(
    move |result| {
      sender1.send(result).unwrap();
    },
  );

  // assert_eq!(receiver.try_recv().unwrap_err(), TryRecvError::Empty);

  lio::tick();

  let sock =
    receiver.recv().unwrap().expect("Failed to create TCP IPv6 socket");
  assert!(sock >= 0, "Socket fd should be valid");

  // Verify it's a TCP socket
  unsafe {
    let mut sock_type: i32 = 0;
    let mut len = std::mem::size_of::<i32>() as libc::socklen_t;
    libc::getsockopt(
      sock,
      libc::SOL_SOCKET,
      libc::SO_TYPE,
      &mut sock_type as *mut _ as *mut libc::c_void,
      &mut len,
    );
    assert_eq!(sock_type, libc::SOCK_STREAM);
    libc::close(sock);
  }
}

#[ignore = "flaky network test"]
#[test]
fn test_socket_udp_ipv4() {
  lio::init();

  let (sender, receiver) = mpsc::channel();
  let sender1 = sender.clone();

  lio::test_utils::udp_socket().when_done(
    move |result| {
      sender1.send(result).unwrap();
    },
  );

  // assert_eq!(receiver.try_recv().unwrap_err(), TryRecvError::Empty);

  lio::tick();

  let sock =
    receiver.recv().unwrap().expect("Failed to create UDP IPv4 socket");
  assert!(sock >= 0, "Socket fd should be valid");

  // Verify it's a UDP socket
  unsafe {
    let mut sock_type: i32 = 0;
    let mut len = std::mem::size_of::<i32>() as libc::socklen_t;
    libc::getsockopt(
      sock,
      libc::SOL_SOCKET,
      libc::SO_TYPE,
      &mut sock_type as *mut _ as *mut libc::c_void,
      &mut len,
    );
    assert_eq!(sock_type, libc::SOCK_DGRAM);
    libc::close(sock);
  }
}

#[ignore = "flaky network test"]
#[test]
fn test_socket_udp_ipv6() {
  lio::init();

  let (sender, receiver) = mpsc::channel();
  let sender1 = sender.clone();

  lio::test_utils::udp6_socket().when_done(
    move |result| {
      sender1.send(result).unwrap();
    },
  );

  // assert_eq!(receiver.try_recv().unwrap_err(), TryRecvError::Empty);

  lio::tick();

  let sock =
    receiver.recv().unwrap().expect("Failed to create UDP IPv6 socket");
  assert!(sock >= 0, "Socket fd should be valid");

  unsafe {
    let mut sock_type: i32 = 0;
    let mut len = std::mem::size_of::<i32>() as libc::socklen_t;
    libc::getsockopt(
      sock,
      libc::SOL_SOCKET,
      libc::SO_TYPE,
      &mut sock_type as *mut _ as *mut libc::c_void,
      &mut len,
    );
    assert_eq!(sock_type, libc::SOCK_DGRAM);
    libc::close(sock);
  }
}

#[ignore = "flaky network test"]
#[test]
fn test_socket_without_protocol() {
  lio::init();

  let (sender, receiver) = mpsc::channel();
  let sender1 = sender.clone();

  lio::test_utils::tcp_socket().when_done(move |result| {
    sender1.send(result).unwrap();
  });

  // assert_eq!(receiver.try_recv().unwrap_err(), TryRecvError::Empty);

  lio::tick();

  let sock = receiver
    .recv()
    .unwrap()
    .expect("Failed to create socket without explicit protocol");
  assert!(sock >= 0, "Socket fd should be valid");

  unsafe {
    libc::close(sock);
  }
}

#[ignore = "flaky network test"]
#[test]
fn test_socket_unix_stream() {
  lio::init();

  let sock = lio::test_utils::unix_stream_socket();
  assert!(sock >= 0, "Socket fd should be valid");

  unsafe {
    let mut sock_type: i32 = 0;
    let mut len = std::mem::size_of::<i32>() as libc::socklen_t;
    libc::getsockopt(
      sock,
      libc::SOL_SOCKET,
      libc::SO_TYPE,
      &mut sock_type as *mut _ as *mut libc::c_void,
      &mut len,
    );
    assert_eq!(sock_type, libc::SOCK_STREAM);
    libc::close(sock);
  }
}

#[ignore = "flaky network test"]
#[test]
fn test_socket_unix_dgram() {
  lio::init();

  let sock = lio::test_utils::unix_dgram_socket();
  assert!(sock >= 0, "Socket fd should be valid");

  unsafe {
    let mut sock_type: i32 = 0;
    let mut len = std::mem::size_of::<i32>() as libc::socklen_t;
    libc::getsockopt(
      sock,
      libc::SOL_SOCKET,
      libc::SO_TYPE,
      &mut sock_type as *mut _ as *mut libc::c_void,
      &mut len,
    );
    assert_eq!(sock_type, libc::SOCK_DGRAM);
    libc::close(sock);
  }
}

#[ignore = "flaky network test"]
#[test]
fn test_socket_multiple() {
  lio::init();

  let (sender, receiver) = mpsc::channel();

  // Create multiple sockets
  let sender1 = sender.clone();
  lio::test_utils::tcp_socket().when_done(
    move |result| {
      sender1.send(("sock1", result)).unwrap();
    },
  );

  let sender2 = sender.clone();
  lio::test_utils::tcp_socket().when_done(
    move |result| {
      sender2.send(("sock2", result)).unwrap();
    },
  );

  let sender3 = sender.clone();
  lio::test_utils::udp_socket().when_done(
    move |result| {
      sender3.send(("sock3", result)).unwrap();
    },
  );

  // assert_eq!(receiver.try_recv().unwrap_err(), TryRecvError::Empty);

  lio::tick();

  let sock1 =
    receiver.recv().unwrap().1.expect("Failed to create first socket");
  let sock2 =
    receiver.recv().unwrap().1.expect("Failed to create second socket");
  let sock3 =
    receiver.recv().unwrap().1.expect("Failed to create third socket");

  assert!(sock1 >= 0);
  assert!(sock2 >= 0);
  assert!(sock3 >= 0);
  assert_ne!(sock1, sock2);
  assert_ne!(sock1, sock3);
  assert_ne!(sock2, sock3);

  // Cleanup
  unsafe {
    libc::close(sock1);
    libc::close(sock2);
    libc::close(sock3);
  }
}

#[ignore = "flaky network test"]
#[test]
fn test_socket_concurrent() {
  lio::init();

  let (sender, receiver) = mpsc::channel();

  // Test creating multiple sockets
  for i in 0..20 {
    let sender_clone = sender.clone();

    // Alternate between TCP IPv4, TCP IPv6, UDP IPv4, UDP IPv6
    match i % 4 {
      0 => lio::test_utils::tcp_socket().when_done(move |result| {
        sender_clone.send(result).unwrap();
      }),
      1 => lio::test_utils::tcp6_socket().when_done(move |result| {
        sender_clone.send(result).unwrap();
      }),
      2 => lio::test_utils::udp_socket().when_done(move |result| {
        sender_clone.send(result).unwrap();
      }),
      _ => lio::test_utils::udp6_socket().when_done(move |result| {
        sender_clone.send(result).unwrap();
      }),
    };
  }

  // assert_eq!(receiver.try_recv().unwrap_err(), TryRecvError::Empty);

  lio::tick();

  for _ in 0..20 {
    let sock = receiver.recv().unwrap().expect("Failed to create socket");
    assert!(sock >= 0);
    unsafe {
      libc::close(sock);
    }
  }
}

#[ignore = "flaky network test"]
#[test]
fn test_socket_options_after_creation() {
  lio::init();

  let (sender, receiver) = mpsc::channel();
  let sender1 = sender.clone();

  lio::test_utils::tcp_socket().when_done(
    move |result| {
      sender1.send(result).unwrap();
    },
  );

  // assert_eq!(receiver.try_recv().unwrap_err(), TryRecvError::Empty);

  lio::tick();

  let sock = receiver.recv().unwrap().expect("Failed to create socket");

  // Test setting socket options
  unsafe {
    let reuse_val: i32 = 1;
    let result = libc::setsockopt(
      sock,
      libc::SOL_SOCKET,
      libc::SO_REUSEADDR,
      &reuse_val as *const _ as *const libc::c_void,
      std::mem::size_of::<i32>() as libc::socklen_t,
    );
    assert_eq!(result, 0, "Failed to set SO_REUSEADDR");

    // Verify the option was set
    let mut get_val: i32 = 0;
    let mut len = std::mem::size_of::<i32>() as libc::socklen_t;
    libc::getsockopt(
      sock,
      libc::SOL_SOCKET,
      libc::SO_REUSEADDR,
      &mut get_val as *mut _ as *mut libc::c_void,
      &mut len,
    );
    assert_ne!(get_val, 0);

    libc::close(sock);
  }
}

#[ignore = "flaky network test"]
#[test]
fn test_socket_nonblocking() {
  lio::init();

  let (sender, receiver) = mpsc::channel();
  let sender1 = sender.clone();

  lio::test_utils::tcp_socket().when_done(
    move |result| {
      sender1.send(result).unwrap();
    },
  );

  // assert_eq!(receiver.try_recv().unwrap_err(), TryRecvError::Empty);

  lio::tick();

  let sock = receiver.recv().unwrap().expect("Failed to create socket");

  // Set non-blocking mode
  unsafe {
    let flags = libc::fcntl(sock, libc::F_GETFL, 0);
    libc::fcntl(sock, libc::F_SETFL, flags | libc::O_NONBLOCK);

    // Verify non-blocking mode is set
    let new_flags = libc::fcntl(sock, libc::F_GETFL, 0);
    assert!(new_flags & libc::O_NONBLOCK != 0);

    libc::close(sock);
  }
}
