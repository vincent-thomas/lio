use lio::Lio;
use lio::api::bind;
use std::{
  net::SocketAddr,
  os::fd::{AsFd, AsRawFd},
  sync::mpsc,
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
fn test_bind_ipv4_any_port() {
  let mut lio = Lio::new(64).unwrap();

  let (sender_sock, receiver_sock) = mpsc::channel();
  let (sender_bind, receiver_bind) = mpsc::channel();

  lio::test_utils::tcp_socket()
    .with_lio(&mut lio)
    .send_with(sender_sock.clone());

  let sock =
    poll_until_recv(&mut lio, &receiver_sock).expect("Failed to create socket");

  // Bind to 0.0.0.0:0 (any available port)
  let addr: SocketAddr = "0.0.0.0:0".parse().unwrap();

  bind(&sock, addr).with_lio(&mut lio).send_with(sender_bind.clone());

  poll_until_recv(&mut lio, &receiver_bind).expect("Failed to bind socket");

  // Verify binding by getting the socket name
  unsafe {
    let mut addr_storage =
      std::mem::MaybeUninit::<libc::sockaddr_storage>::zeroed();
    let mut addr_len =
      std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    let result = libc::getsockname(
      sock.clone().as_fd().as_raw_fd(),
      addr_storage.as_mut_ptr() as *mut libc::sockaddr,
      &mut addr_len,
    );
    assert_eq!(result, 0, "getsockname should succeed");
    libc::close(sock.as_fd().as_raw_fd());
  }
}

#[test]
fn test_bind_ipv4_specific_port() {
  let mut lio = Lio::new(64).unwrap();

  let (sender_sock, receiver_sock) = mpsc::channel();
  let (sender_bind, receiver_bind) = mpsc::channel();

  lio::test_utils::tcp_socket()
    .with_lio(&mut lio)
    .send_with(sender_sock.clone());

  let sock =
    poll_until_recv(&mut lio, &receiver_sock).expect("Failed to create socket");

  // Bind to a high port number
  let addr: SocketAddr = "127.0.0.1:19999".parse().unwrap();

  bind(&sock, addr).with_lio(&mut lio).send_with(sender_bind.clone());

  poll_until_recv(&mut lio, &receiver_bind)
    .expect("Failed to bind to specific port");

  // Verify the port
  unsafe {
    let mut addr_storage = std::mem::MaybeUninit::<libc::sockaddr_in>::zeroed();
    let mut addr_len =
      std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
    libc::getsockname(
      sock.as_fd().as_raw_fd(),
      addr_storage.as_mut_ptr() as *mut libc::sockaddr,
      &mut addr_len,
    );
    let sockaddr_in = addr_storage.assume_init();
    assert_eq!(u16::from_be(sockaddr_in.sin_port), 19999);
    libc::close(sock.as_fd().as_raw_fd());
  }
}

#[test]
fn test_bind_ipv6() {
  let mut lio = Lio::new(64).unwrap();

  let (sender_sock, receiver_sock) = mpsc::channel();
  let (sender_bind, receiver_bind) = mpsc::channel();

  lio::test_utils::tcp6_socket()
    .with_lio(&mut lio)
    .send_with(sender_sock.clone());

  let sock = poll_until_recv(&mut lio, &receiver_sock)
    .expect("Failed to create IPv6 socket");

  // Bind to IPv6 any address
  let addr: SocketAddr = "[::]:0".parse().unwrap();

  bind(&sock, addr).with_lio(&mut lio).send_with(sender_bind.clone());

  poll_until_recv(&mut lio, &receiver_bind)
    .expect("Failed to bind IPv6 socket");

  // Verify binding
  unsafe {
    let mut addr_storage =
      std::mem::MaybeUninit::<libc::sockaddr_storage>::zeroed();
    let mut addr_len =
      std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    let result = libc::getsockname(
      sock.as_fd().as_raw_fd(),
      addr_storage.as_mut_ptr() as *mut libc::sockaddr,
      &mut addr_len,
    );
    assert_eq!(result, 0);
    libc::close(sock.as_fd().as_raw_fd());
  }
}

#[test]
fn test_bind_udp() {
  let mut lio = Lio::new(64).unwrap();

  let (sender_sock, receiver_sock) = mpsc::channel();
  let (sender_bind, receiver_bind) = mpsc::channel();

  lio::test_utils::udp_socket()
    .with_lio(&mut lio)
    .send_with(sender_sock.clone());

  let sock = poll_until_recv(&mut lio, &receiver_sock)
    .expect("Failed to create UDP socket");

  let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();

  bind(&sock, addr).with_lio(&mut lio).send_with(sender_bind.clone());

  poll_until_recv(&mut lio, &receiver_bind).expect("Failed to bind UDP socket");

  // Verify binding
  unsafe {
    let mut addr_storage =
      std::mem::MaybeUninit::<libc::sockaddr_storage>::zeroed();
    let mut addr_len =
      std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    let result = libc::getsockname(
      sock.as_raw_fd(),
      addr_storage.as_mut_ptr() as *mut libc::sockaddr,
      &mut addr_len,
    );
    assert_eq!(result, 0);
    libc::close(sock.as_raw_fd());
  }
}

#[test]
fn test_bind_already_bound() {
  let mut lio = Lio::new(64).unwrap();

  let (sender_sock, receiver_sock) = mpsc::channel();
  let (sender_bind, receiver_bind) = mpsc::channel();

  lio::test_utils::tcp_socket()
    .with_lio(&mut lio)
    .send_with(sender_sock.clone());

  let sock1 = poll_until_recv(&mut lio, &receiver_sock)
    .expect("Failed to create first socket");

  let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();

  bind(&sock1, addr).with_lio(&mut lio).send_with(sender_bind.clone());

  poll_until_recv(&mut lio, &receiver_bind)
    .expect("Failed to bind first socket");

  // Get the actual bound address
  let bound_addr = unsafe {
    let mut addr_storage = std::mem::MaybeUninit::<libc::sockaddr_in>::zeroed();
    let mut addr_len =
      std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
    libc::getsockname(
      sock1.as_raw_fd(),
      addr_storage.as_mut_ptr() as *mut libc::sockaddr,
      &mut addr_len,
    );
    addr_storage.assume_init()
  };

  // Try to bind another socket to the same address
  lio::test_utils::tcp_socket()
    .with_lio(&mut lio)
    .send_with(sender_sock.clone());

  let sock2 = poll_until_recv(&mut lio, &receiver_sock)
    .expect("Failed to create second socket");

  let port = u16::from_be(bound_addr.sin_port);
  let addr2: SocketAddr = format!("127.0.0.1:{}", port).parse().unwrap();

  let (sender_bind2, receiver_bind2) = mpsc::channel();
  bind(&sock2, addr2).with_lio(&mut lio).send_with(sender_bind2.clone());

  let result = poll_until_recv(&mut lio, &receiver_bind2);

  // Should fail with address in use
  assert!(result.is_err(), "Binding to already-used address should fail");

  // Cleanup
  unsafe {
    libc::close(sock1.as_raw_fd());
    libc::close(sock2.as_raw_fd());
  }
}

#[test]
fn test_bind_double_bind() {
  let mut lio = Lio::new(64).unwrap();

  let (sender_sock, receiver_sock) = mpsc::channel();
  let (sender_bind, receiver_bind) = mpsc::channel();

  lio::test_utils::tcp_socket()
    .with_lio(&mut lio)
    .send_with(sender_sock.clone());

  let sock =
    poll_until_recv(&mut lio, &receiver_sock).expect("Failed to create socket");

  let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();

  bind(&sock, addr).with_lio(&mut lio).send_with(sender_bind.clone());

  poll_until_recv(&mut lio, &receiver_bind).expect("First bind should succeed");

  // Try to bind the same socket again
  let (sender_bind2, receiver_bind2) = mpsc::channel();
  bind(&sock, addr).with_lio(&mut lio).send_with(sender_bind2);

  let result = poll_until_recv(&mut lio, &receiver_bind2);

  // Should fail
  assert!(result.is_err(), "Double bind should fail");

  // Cleanup
  unsafe {
    libc::close(sock.as_raw_fd());
  }
}

#[test]
fn test_bind_with_reuseaddr() {
  let mut lio = Lio::new(64).unwrap();

  let (sender_sock, receiver_sock) = mpsc::channel();
  let (sender_bind, receiver_bind) = mpsc::channel();

  lio::test_utils::tcp_socket()
    .with_lio(&mut lio)
    .send_with(sender_sock.clone());

  let sock1 = poll_until_recv(&mut lio, &receiver_sock)
    .expect("Failed to create first socket");

  // Enable SO_REUSEADDR on first socket
  unsafe {
    let reuse: i32 = 1;
    libc::setsockopt(
      sock1.as_raw_fd(),
      libc::SOL_SOCKET,
      libc::SO_REUSEADDR,
      &reuse as *const _ as *const libc::c_void,
      std::mem::size_of::<i32>() as libc::socklen_t,
    );
  }

  let addr: SocketAddr = "127.0.0.1:18888".parse().unwrap();

  bind(&sock1, addr).with_lio(&mut lio).send_with(sender_bind.clone());

  poll_until_recv(&mut lio, &receiver_bind)
    .expect("Failed to bind first socket");

  // Close first socket
  unsafe {
    libc::close(sock1.as_raw_fd());
  }

  // Immediately bind another socket to the same address with SO_REUSEADDR
  lio::test_utils::tcp_socket()
    .with_lio(&mut lio)
    .send_with(sender_sock.clone());

  let sock2 = poll_until_recv(&mut lio, &receiver_sock)
    .expect("Failed to create second socket");

  unsafe {
    let reuse: i32 = 1;
    libc::setsockopt(
      sock2.as_raw_fd(),
      libc::SOL_SOCKET,
      libc::SO_REUSEADDR,
      &reuse as *const _ as *const libc::c_void,
      std::mem::size_of::<i32>() as libc::socklen_t,
    );
  }

  let (sender_bind2, receiver_bind2) = mpsc::channel();
  bind(&sock2, addr).with_lio(&mut lio).send_with(sender_bind2.clone());

  poll_until_recv(&mut lio, &receiver_bind2).expect(
    "Should be able to bind with SO_REUSEADDR after closing previous socket",
  );

  // Cleanup
  unsafe {
    libc::close(sock2.as_raw_fd());
  }
}

#[test]
fn test_bind_localhost() {
  let mut lio = Lio::new(64).unwrap();

  let (sender_sock, receiver_sock) = mpsc::channel();
  let (sender_bind, receiver_bind) = mpsc::channel();

  lio::test_utils::tcp_socket()
    .with_lio(&mut lio)
    .send_with(sender_sock.clone());

  let sock =
    poll_until_recv(&mut lio, &receiver_sock).expect("Failed to create socket");

  let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();

  bind(&sock, addr).with_lio(&mut lio).send_with(sender_bind.clone());

  poll_until_recv(&mut lio, &receiver_bind)
    .expect("Failed to bind to localhost");

  // Verify it's bound to localhost
  unsafe {
    let mut addr_storage = std::mem::MaybeUninit::<libc::sockaddr_in>::zeroed();
    let mut addr_len =
      std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
    libc::getsockname(
      sock.as_raw_fd(),
      addr_storage.as_mut_ptr() as *mut libc::sockaddr,
      &mut addr_len,
    );
    let sockaddr_in = addr_storage.assume_init();
    // 127.0.0.1 in network byte order
    assert_eq!(u32::from_be(sockaddr_in.sin_addr.s_addr), 0x7f000001);
    libc::close(sock.as_raw_fd());
  }
}

#[test]
fn test_bind_concurrent() {
  let mut lio = Lio::new(64).unwrap();

  let (sender_sock, receiver_sock) = mpsc::channel();
  let (sender_bind, receiver_bind) = mpsc::channel();

  // Test binding multiple sockets concurrently to different ports
  for _ in 20000..20010 {
    lio::test_utils::tcp_socket()
      .with_lio(&mut lio)
      .send_with(sender_sock.clone());
  }

  let mut socks = Vec::new();
  for _ in 0..10 {
    let sock = poll_until_recv(&mut lio, &receiver_sock)
      .expect("Failed to create socket");
    socks.push(sock);
  }

  // Set SO_REUSEADDR and bind each socket
  for (i, sock) in socks.iter().enumerate() {
    let port = 20000 + i;
    let addr: SocketAddr = format!("127.0.0.1:{}", port).parse().unwrap();
    bind(sock, addr).with_lio(&mut lio).send_with(sender_bind.clone());
  }

  for _ in 0..10 {
    poll_until_recv(&mut lio, &receiver_bind).expect("Failed to bind socket");
  }

  // Cleanup
  for sock in socks {
    unsafe {
      libc::close(sock.as_raw_fd());
    }
  }
}
