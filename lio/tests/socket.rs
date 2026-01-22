use lio::Lio;
use std::{
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
fn test_socket_simple() {
  let mut lio = Lio::new(64).unwrap();

  let (sender, receiver) = mpsc::channel();

  lio::test_utils::tcp_socket().with_lio(&mut lio).send_with(sender.clone());

  let sock = poll_until_recv(&mut lio, &receiver)
    .expect("Failed to create TCP IPv4 socket");

  // Verify it's a TCP socket
  let raw_fd = sock.as_fd().as_raw_fd();
  assert!(raw_fd >= 0, "Socket fd should be valid");

  let mut sock_type: i32 = 0;
  let mut len = std::mem::size_of::<i32>() as libc::socklen_t;
  unsafe {
    libc::getsockopt(
      raw_fd,
      libc::SOL_SOCKET,
      libc::SO_TYPE,
      &mut sock_type as *mut _ as *mut libc::c_void,
      &mut len,
    );
    assert_eq!(sock_type, libc::SOCK_STREAM);
  }

  assert!(sock.will_close());

  drop(sock);
}

#[test]
fn test_socket_tcp_ipv4() {
  let mut lio = Lio::new(64).unwrap();

  let (sender, receiver) = mpsc::channel();

  lio::test_utils::tcp_socket().with_lio(&mut lio).send_with(sender);

  let sock = poll_until_recv(&mut lio, &receiver)
    .expect("Failed to create TCP IPv4 socket");
  assert!(sock.as_fd().as_raw_fd() >= 0, "Socket fd should be valid");

  // Verify it's a TCP socket
  unsafe {
    let mut sock_type: i32 = 0;
    let mut len = std::mem::size_of::<i32>() as libc::socklen_t;
    libc::getsockopt(
      sock.as_fd().as_raw_fd(),
      libc::SOL_SOCKET,
      libc::SO_TYPE,
      &mut sock_type as *mut _ as *mut libc::c_void,
      &mut len,
    );
    assert_eq!(sock_type, libc::SOCK_STREAM);
    libc::close(sock.as_fd().as_raw_fd());
  }
}

#[test]
fn test_socket_tcp_ipv6() {
  let mut lio = Lio::new(64).unwrap();

  let (sender, receiver) = mpsc::channel();

  lio::test_utils::tcp6_socket().with_lio(&mut lio).send_with(sender);

  let sock = poll_until_recv(&mut lio, &receiver)
    .expect("Failed to create TCP IPv6 socket");
  assert!(sock.as_fd().as_raw_fd() >= 0, "Socket fd should be valid");

  // Verify it's a TCP socket
  unsafe {
    let mut sock_type: i32 = 0;
    let mut len = std::mem::size_of::<i32>() as libc::socklen_t;
    libc::getsockopt(
      sock.as_fd().as_raw_fd(),
      libc::SOL_SOCKET,
      libc::SO_TYPE,
      &mut sock_type as *mut _ as *mut libc::c_void,
      &mut len,
    );
    assert_eq!(sock_type, libc::SOCK_STREAM);
    libc::close(sock.as_fd().as_raw_fd());
  }
}

#[test]
fn test_socket_udp_ipv4() {
  let mut lio = Lio::new(64).unwrap();

  let (sender, receiver) = mpsc::channel();

  lio::test_utils::udp_socket().with_lio(&mut lio).send_with(sender);

  let sock = poll_until_recv(&mut lio, &receiver)
    .expect("Failed to create UDP IPv4 socket");
  assert!(sock.as_fd().as_raw_fd() >= 0, "Socket fd should be valid");

  // Verify it's a UDP socket
  unsafe {
    let mut sock_type: i32 = 0;
    let mut len = std::mem::size_of::<i32>() as libc::socklen_t;
    libc::getsockopt(
      sock.as_fd().as_raw_fd(),
      libc::SOL_SOCKET,
      libc::SO_TYPE,
      &mut sock_type as *mut _ as *mut libc::c_void,
      &mut len,
    );
    assert_eq!(sock_type, libc::SOCK_DGRAM);
    libc::close(sock.as_fd().as_raw_fd());
  }
}

#[test]
fn test_socket_udp_ipv6() {
  let mut lio = Lio::new(64).unwrap();

  let (sender, receiver) = mpsc::channel();

  lio::test_utils::udp6_socket().with_lio(&mut lio).send_with(sender);

  let sock = poll_until_recv(&mut lio, &receiver)
    .expect("Failed to create UDP IPv6 socket");
  assert!(sock.as_fd().as_raw_fd() >= 0, "Socket fd should be valid");

  unsafe {
    let mut sock_type: i32 = 0;
    let mut len = std::mem::size_of::<i32>() as libc::socklen_t;
    libc::getsockopt(
      sock.as_fd().as_raw_fd(),
      libc::SOL_SOCKET,
      libc::SO_TYPE,
      &mut sock_type as *mut _ as *mut libc::c_void,
      &mut len,
    );
    assert_eq!(sock_type, libc::SOCK_DGRAM);
    libc::close(sock.as_fd().as_raw_fd());
  }
}

#[test]
fn test_socket_without_protocol() {
  let mut lio = Lio::new(64).unwrap();

  let (sender, receiver) = mpsc::channel();

  lio::test_utils::tcp_socket().with_lio(&mut lio).send_with(sender);

  let sock = poll_until_recv(&mut lio, &receiver)
    .expect("Failed to create socket without explicit protocol");
  assert!(sock.as_fd().as_raw_fd() >= 0, "Socket fd should be valid");

  unsafe {
    libc::close(sock.as_fd().as_raw_fd());
  }
}

#[test]
fn test_socket_unix_stream() {
  let mut lio = Lio::new(64).unwrap();
  let mut sock_recv =
    lio::test_utils::unix_stream_socket().with_lio(&mut lio).send();

  assert_eq!(lio.try_run().unwrap(), 1);

  let socket = sock_recv.try_recv().unwrap().unwrap();
  assert!(socket.as_fd().as_raw_fd() >= 0, "Socket fd should be valid");

  unsafe {
    let mut sock_type: i32 = 0;
    let mut len = std::mem::size_of::<i32>() as libc::socklen_t;
    libc::getsockopt(
      socket.as_fd().as_raw_fd(),
      libc::SOL_SOCKET,
      libc::SO_TYPE,
      &mut sock_type as *mut _ as *mut libc::c_void,
      &mut len,
    );
    assert_eq!(sock_type, libc::SOCK_STREAM);
    libc::close(socket.as_fd().as_raw_fd());
  }
}

#[test]
fn test_socket_unix_dgram() {
  let mut lio = Lio::new(64).unwrap();
  let mut sock_recv =
    lio::test_utils::unix_dgram_socket().with_lio(&mut lio).send();

  assert_eq!(lio.try_run().unwrap(), 1);

  let sock = sock_recv.try_recv().unwrap().unwrap();
  assert!(sock.as_fd().as_raw_fd() >= 0, "Socket fd should be valid");

  unsafe {
    let mut sock_type: i32 = 0;
    let mut len = std::mem::size_of::<i32>() as libc::socklen_t;
    libc::getsockopt(
      sock.as_fd().as_raw_fd(),
      libc::SOL_SOCKET,
      libc::SO_TYPE,
      &mut sock_type as *mut _ as *mut libc::c_void,
      &mut len,
    );
    assert_eq!(sock_type, libc::SOCK_DGRAM);
    libc::close(sock.as_fd().as_raw_fd());
  }
}

#[test]
fn test_socket_multiple() {
  let mut lio = Lio::new(64).unwrap();

  let (sender, receiver) = mpsc::channel();

  // Create multiple sockets
  lio::test_utils::tcp_socket().with_lio(&mut lio).send_with(sender.clone());

  lio::test_utils::tcp_socket().with_lio(&mut lio).send_with(sender.clone());

  lio::test_utils::udp_socket().with_lio(&mut lio).send_with(sender.clone());

  let sock1 = poll_until_recv(&mut lio, &receiver)
    .expect("Failed to create first socket");
  let sock2 = poll_until_recv(&mut lio, &receiver)
    .expect("Failed to create second socket");
  let sock3 = poll_until_recv(&mut lio, &receiver)
    .expect("Failed to create third socket");

  assert!(sock1.as_fd().as_raw_fd() >= 0);
  assert!(sock2.as_fd().as_raw_fd() >= 0);
  assert!(sock3.as_fd().as_raw_fd() >= 0);
  assert_ne!(sock1.as_fd().as_raw_fd(), sock2.as_fd().as_raw_fd());
  assert_ne!(sock1.as_fd().as_raw_fd(), sock3.as_fd().as_raw_fd());
  assert_ne!(sock2.as_fd().as_raw_fd(), sock3.as_fd().as_raw_fd());

  // Cleanup
  unsafe {
    libc::close(sock1.as_fd().as_raw_fd());
    libc::close(sock2.as_fd().as_raw_fd());
    libc::close(sock3.as_fd().as_raw_fd());
  }
}

#[test]
fn test_socket_concurrent() {
  let mut lio = Lio::new(64).unwrap();

  let (sender, receiver) = mpsc::channel();

  // Test creating multiple sockets
  for i in 0..20 {
    // Alternate between TCP IPv4, TCP IPv6, UDP IPv4, UDP IPv6
    match i % 4 {
      0 => lio::test_utils::tcp_socket()
        .with_lio(&mut lio)
        .send_with(sender.clone()),
      1 => lio::test_utils::tcp6_socket()
        .with_lio(&mut lio)
        .send_with(sender.clone()),
      2 => lio::test_utils::udp_socket()
        .with_lio(&mut lio)
        .send_with(sender.clone()),
      _ => lio::test_utils::udp6_socket()
        .with_lio(&mut lio)
        .send_with(sender.clone()),
    };
  }

  for _ in 0..20 {
    let sock =
      poll_until_recv(&mut lio, &receiver).expect("Failed to create socket");
    assert!(sock.as_fd().as_raw_fd() >= 0);
    unsafe {
      libc::close(sock.as_fd().as_raw_fd());
    }
  }
}

#[test]
fn test_socket_options_after_creation() {
  let mut lio = Lio::new(64).unwrap();

  let (sender, receiver) = mpsc::channel();

  lio::test_utils::tcp_socket().with_lio(&mut lio).send_with(sender);

  let sock =
    poll_until_recv(&mut lio, &receiver).expect("Failed to create socket");

  // Test setting socket options
  unsafe {
    let reuse_val: i32 = 1;
    let result = libc::setsockopt(
      sock.as_fd().as_raw_fd(),
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
      sock.as_fd().as_raw_fd(),
      libc::SOL_SOCKET,
      libc::SO_REUSEADDR,
      &mut get_val as *mut _ as *mut libc::c_void,
      &mut len,
    );
    assert_ne!(get_val, 0);

    libc::close(sock.as_fd().as_raw_fd());
  }
}

#[test]
fn test_socket_nonblocking() {
  let mut lio = Lio::new(64).unwrap();

  let (sender, receiver) = mpsc::channel();

  lio::test_utils::tcp_socket().with_lio(&mut lio).send_with(sender);

  let sock =
    poll_until_recv(&mut lio, &receiver).expect("Failed to create socket");

  // Set non-blocking mode
  unsafe {
    use std::os::fd::AsRawFd;
    let raw_fd = sock.as_fd().as_raw_fd();
    let flags = libc::fcntl(raw_fd, libc::F_GETFL, 0);
    libc::fcntl(raw_fd, libc::F_SETFL, flags | libc::O_NONBLOCK);

    // Verify non-blocking mode is set
    let new_flags = libc::fcntl(raw_fd, libc::F_GETFL, 0);
    assert!(new_flags & libc::O_NONBLOCK != 0);

    libc::close(raw_fd);
  }
}
