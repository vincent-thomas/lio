use lio::api::*;
use lio::Lio;
use std::{
  mem::MaybeUninit,
  net::SocketAddr,
  os::fd::{AsFd, AsRawFd},
  sync::mpsc::{self},
  thread,
  time::Duration,
};

/// Helper to poll until we receive a result
fn poll_until_recv<T>(lio: &mut Lio, receiver: &mpsc::Receiver<T>) -> T {
  let mut attempts = 0;
  loop {
    lio.try_run().unwrap();
    match receiver.try_recv() {
      Ok(result) => return result,
      Err(mpsc::TryRecvError::Empty) => {
        attempts += 1;
        if attempts > 10 {
          panic!("Operation did not complete after 10 attempts");
        }
        thread::sleep(Duration::from_micros(100));
      }
      Err(mpsc::TryRecvError::Disconnected) => {
        panic!("Channel disconnected");
      }
    }
  }
}

#[test]
fn test_connect_basic() {
  let mut lio = Lio::new(64).unwrap();

  let (sender_sock, receiver_sock) = mpsc::channel();
  let (sender_unit, receiver_unit) = mpsc::channel();

  lio::test_utils::tcp_socket()
    .with_lio(&mut lio)
    .send_with(sender_sock.clone());

  let server_sock =
    poll_until_recv(&mut lio, &receiver_sock).expect("Failed to create server socket");

  let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();

  bind(&server_sock, addr)
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

  listen(&server_sock, 128)
    .with_lio(&mut lio)
    .send_with(sender_unit.clone());

  poll_until_recv(&mut lio, &receiver_unit).expect("Failed to listen");

  // Create client socket and connect
  lio::test_utils::tcp_socket()
    .with_lio(&mut lio)
    .send_with(sender_sock.clone());

  let client_sock =
    poll_until_recv(&mut lio, &receiver_sock).expect("Failed to create client socket");

  let (sender_c, receiver_c) = mpsc::channel();

  connect(&client_sock, bound_addr)
    .with_lio(&mut lio)
    .send_with(sender_c.clone());

  poll_until_recv(&mut lio, &receiver_c).expect("Failed to connect");

  // Verify connection by checking peer name
  unsafe {
    let mut peer_addr = MaybeUninit::<libc::sockaddr_storage>::zeroed();
    let mut peer_len =
      std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    let result = libc::getpeername(
      client_sock.as_fd().as_raw_fd(),
      peer_addr.as_mut_ptr() as *mut libc::sockaddr,
      &mut peer_len,
    );
    assert_eq!(result, 0, "Should be able to get peer name after connect");

    libc::close(client_sock.as_fd().as_raw_fd());
    libc::close(server_sock.as_fd().as_raw_fd());
  }
}

#[test]
fn test_connect_ipv6() {
  let mut lio = Lio::new(64).unwrap();

  let (sender_sock, receiver_sock) = mpsc::channel();
  let (sender_unit, receiver_unit) = mpsc::channel();

  lio::test_utils::tcp6_socket()
    .with_lio(&mut lio)
    .send_with(sender_sock.clone());

  let server_sock =
    poll_until_recv(&mut lio, &receiver_sock).expect("Failed to create IPv6 server socket");

  let addr: SocketAddr = "[::1]:0".parse().unwrap();

  bind(&server_sock, addr)
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

  listen(&server_sock, 128)
    .with_lio(&mut lio)
    .send_with(sender_unit.clone());

  poll_until_recv(&mut lio, &receiver_unit).expect("Failed to listen");

  lio::test_utils::tcp6_socket()
    .with_lio(&mut lio)
    .send_with(sender_sock.clone());

  let client_sock =
    poll_until_recv(&mut lio, &receiver_sock).expect("Failed to create IPv6 client socket");

  let (sender_c, receiver_c) = mpsc::channel();

  connect(&client_sock, bound_addr)
    .with_lio(&mut lio)
    .send_with(sender_c.clone());

  poll_until_recv(&mut lio, &receiver_c).expect("Failed to connect IPv6");

  unsafe {
    let mut peer_addr = MaybeUninit::<libc::sockaddr_storage>::zeroed();
    let mut peer_len =
      std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    let result = libc::getpeername(
      client_sock.as_fd().as_raw_fd(),
      peer_addr.as_mut_ptr() as *mut libc::sockaddr,
      &mut peer_len,
    );
    assert_eq!(result, 0);

    libc::close(client_sock.as_fd().as_raw_fd());
    libc::close(server_sock.as_fd().as_raw_fd());
  }
}

#[test]
fn test_connect_to_nonexistent() {
  let mut lio = Lio::new(64).unwrap();

  let (sender, receiver) = mpsc::channel();

  lio::test_utils::tcp_socket()
    .with_lio(&mut lio)
    .send_with(sender.clone());

  let client_sock =
    poll_until_recv(&mut lio, &receiver).expect("Failed to create client socket");

  // Try to connect to a port that's (hopefully) not listening
  let addr: SocketAddr = "127.0.0.1:1".parse().unwrap();

  let (sender_c, receiver_c) = mpsc::channel();

  connect(&client_sock, addr)
    .with_lio(&mut lio)
    .send_with(sender_c.clone());

  let result = poll_until_recv(&mut lio, &receiver_c);

  // Should fail with connection refused
  assert!(result.is_err(), "Connect to non-listening port should fail");

  unsafe {
    libc::close(client_sock.as_fd().as_raw_fd());
  }
}

#[test]
fn test_connect_multiple_clients() {
  let mut lio = Lio::new(64).unwrap();

  let (sender_sock, receiver_sock) = mpsc::channel();
  let (sender_unit, receiver_unit) = mpsc::channel();

  lio::test_utils::tcp_socket()
    .with_lio(&mut lio)
    .send_with(sender_sock.clone());

  let server_sock =
    poll_until_recv(&mut lio, &receiver_sock).expect("Failed to create server socket");

  let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();

  bind(&server_sock, addr)
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

  listen(&server_sock, 128)
    .with_lio(&mut lio)
    .send_with(sender_unit.clone());

  poll_until_recv(&mut lio, &receiver_unit).expect("Failed to listen");

  // Connect multiple clients
  let mut client_socks = Vec::new();
  for _ in 0..5 {
    lio::test_utils::tcp_socket()
      .with_lio(&mut lio)
      .send_with(sender_sock.clone());

    let client_sock =
      poll_until_recv(&mut lio, &receiver_sock).expect("Failed to create client socket");

    let (sender_c, receiver_c) = mpsc::channel();

    connect(&client_sock, bound_addr)
      .with_lio(&mut lio)
      .send_with(sender_c.clone());

    poll_until_recv(&mut lio, &receiver_c).expect("Failed to connect");

    client_socks.push(client_sock);
  }

  assert_eq!(client_socks.len(), 5);

  // Cleanup
  unsafe {
    for sock in client_socks {
      libc::close(sock.as_fd().as_raw_fd());
    }
    libc::close(server_sock.as_fd().as_raw_fd());
  }
}

#[test]
fn test_connect_already_connected() {
  let mut lio = Lio::new(64).unwrap();

  let (sender_sock, receiver_sock) = mpsc::channel();
  let (sender_unit, receiver_unit) = mpsc::channel();

  lio::test_utils::tcp_socket()
    .with_lio(&mut lio)
    .send_with(sender_sock.clone());

  let server_sock =
    poll_until_recv(&mut lio, &receiver_sock).expect("Failed to create server socket");

  let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();

  bind(&server_sock, addr)
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

  listen(&server_sock, 128)
    .with_lio(&mut lio)
    .send_with(sender_unit.clone());

  poll_until_recv(&mut lio, &receiver_unit).expect("Failed to listen");

  lio::test_utils::tcp_socket()
    .with_lio(&mut lio)
    .send_with(sender_sock.clone());

  let client_sock =
    poll_until_recv(&mut lio, &receiver_sock).expect("Failed to create client socket");

  let (sender_c, receiver_c) = mpsc::channel();

  connect(&client_sock, bound_addr)
    .with_lio(&mut lio)
    .send_with(sender_c.clone());

  poll_until_recv(&mut lio, &receiver_c).expect("First connect should succeed");

  // Try to connect again
  let (sender_c2, receiver_c2) = mpsc::channel();

  connect(&client_sock, bound_addr)
    .with_lio(&mut lio)
    .send_with(sender_c2.clone());

  let result = poll_until_recv(&mut lio, &receiver_c2);

  dbg!(&result);

  // Should fail with already connected
  assert!(result.is_err(), "Second connect should fail: err {result:#?}");

  unsafe {
    libc::close(client_sock.as_fd().as_raw_fd());
    libc::close(server_sock.as_fd().as_raw_fd());
  }
}

#[test]
fn test_connect_to_localhost() {
  let mut lio = Lio::new(64).unwrap();

  let (sender_sock, receiver_sock) = mpsc::channel();
  let (sender_unit, receiver_unit) = mpsc::channel();

  lio::test_utils::tcp_socket()
    .with_lio(&mut lio)
    .send_with(sender_sock.clone());

  let server_sock =
    poll_until_recv(&mut lio, &receiver_sock).expect("Failed to create server socket");

  let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();

  bind(&server_sock, addr)
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

  listen(&server_sock, 128)
    .with_lio(&mut lio)
    .send_with(sender_unit.clone());

  poll_until_recv(&mut lio, &receiver_unit).expect("Failed to listen");

  lio::test_utils::tcp_socket()
    .with_lio(&mut lio)
    .send_with(sender_sock.clone());

  let client_sock =
    poll_until_recv(&mut lio, &receiver_sock).expect("Failed to create client socket");

  let (sender_c, receiver_c) = mpsc::channel();

  connect(&client_sock, bound_addr)
    .with_lio(&mut lio)
    .send_with(sender_c.clone());

  poll_until_recv(&mut lio, &receiver_c).expect("Failed to connect to localhost");

  // Verify connected to localhost
  unsafe {
    let mut peer_addr = MaybeUninit::<libc::sockaddr_in>::zeroed();
    let mut peer_len =
      std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
    libc::getpeername(
      client_sock.as_fd().as_raw_fd(),
      peer_addr.as_mut_ptr() as *mut libc::sockaddr,
      &mut peer_len,
    );
    let sockaddr_in = peer_addr.assume_init();
    assert_eq!(u32::from_be(sockaddr_in.sin_addr.s_addr), 0x7f000001);

    libc::close(client_sock.as_fd().as_raw_fd());
    libc::close(server_sock.as_fd().as_raw_fd());
  }
}

#[test]
fn test_connect_concurrent() {
  let mut lio = Lio::new(64).unwrap();

  let (sender_sock, receiver_sock) = mpsc::channel();
  let (sender_unit, receiver_unit) = mpsc::channel();

  lio::test_utils::tcp_socket()
    .with_lio(&mut lio)
    .send_with(sender_sock.clone());

  let server_sock =
    poll_until_recv(&mut lio, &receiver_sock).expect("Failed to create server socket");

  let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();

  bind(&server_sock, addr)
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

  listen(&server_sock, 128)
    .with_lio(&mut lio)
    .send_with(sender_unit.clone());

  poll_until_recv(&mut lio, &receiver_unit).expect("Failed to listen");

  // Connect multiple clients sequentially
  let mut client_socks = Vec::new();
  for _ in 0..10 {
    lio::test_utils::tcp_socket()
      .with_lio(&mut lio)
      .send_with(sender_sock.clone());

    let client_sock =
      poll_until_recv(&mut lio, &receiver_sock).expect("Failed to create client socket");

    let (sender_c, receiver_c) = mpsc::channel();

    connect(&client_sock, bound_addr)
      .with_lio(&mut lio)
      .send_with(sender_c.clone());

    poll_until_recv(&mut lio, &receiver_c).expect("Failed to connect");

    client_socks.push(client_sock);
  }

  assert_eq!(client_socks.len(), 10);

  // Cleanup
  unsafe {
    for sock in client_socks {
      libc::close(sock.as_fd().as_raw_fd());
    }
    libc::close(server_sock.as_fd().as_raw_fd());
  }
}

#[test]
fn test_connect_with_bind() {
  let mut lio = Lio::new(64).unwrap();

  let (sender_sock, receiver_sock) = mpsc::channel();
  let (sender_unit, receiver_unit) = mpsc::channel();

  lio::test_utils::tcp_socket()
    .with_lio(&mut lio)
    .send_with(sender_sock.clone());

  let server_sock =
    poll_until_recv(&mut lio, &receiver_sock).expect("Failed to create server socket");

  let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();

  bind(&server_sock, addr)
    .with_lio(&mut lio)
    .send_with(sender_unit.clone());

  poll_until_recv(&mut lio, &receiver_unit).expect("Failed to bind server");

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

  listen(&server_sock, 128)
    .with_lio(&mut lio)
    .send_with(sender_unit.clone());

  poll_until_recv(&mut lio, &receiver_unit).expect("Failed to listen");

  // Create client socket and bind it to a specific local address
  lio::test_utils::tcp_socket()
    .with_lio(&mut lio)
    .send_with(sender_sock.clone());

  let client_sock =
    poll_until_recv(&mut lio, &receiver_sock).expect("Failed to create client socket");

  let client_bind_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();

  bind(&client_sock, client_bind_addr)
    .with_lio(&mut lio)
    .send_with(sender_unit.clone());

  poll_until_recv(&mut lio, &receiver_unit).expect("Failed to bind client");

  // Now connect
  let (sender_c, receiver_c) = mpsc::channel();

  connect(&client_sock, bound_addr)
    .with_lio(&mut lio)
    .send_with(sender_c.clone());

  poll_until_recv(&mut lio, &receiver_c).expect("Failed to connect");

  unsafe {
    libc::close(client_sock.as_fd().as_raw_fd());
    libc::close(server_sock.as_fd().as_raw_fd());
  }
}
