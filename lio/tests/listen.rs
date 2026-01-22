use lio::api::{self, bind, listen};
use lio::Lio;
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

#[cfg(linux)]
#[test]
fn test_listen_basic() {
  let mut lio = Lio::new(64).unwrap();

  let (sender_sock, receiver_sock) = mpsc::channel();
  let (sender_unit, receiver_unit) = mpsc::channel::<std::io::Result<()>>();

  lio::test_utils::tcp_socket()
    .with_lio(&mut lio)
    .send_with(sender_sock.clone());

  let sock = poll_until_recv(&mut lio, &receiver_sock).expect("Failed to create socket");

  let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();

  api::bind(&sock, addr)
    .with_lio(&mut lio)
    .send_with(sender_unit.clone());

  poll_until_recv(&mut lio, &receiver_unit).expect("Failed to bind socket");

  api::listen(&sock, 128)
    .with_lio(&mut lio)
    .send_with(sender_unit.clone());

  poll_until_recv(&mut lio, &receiver_unit).expect("Failed to listen on socket");

  // Verify socket is in listening state by checking it accepts connections
  unsafe {
    let mut accept_val: i32 = 0;
    let mut len = std::mem::size_of::<i32>() as libc::socklen_t;
    let res = libc::getsockopt(
      sock.as_raw_fd(),
      libc::SOL_SOCKET,
      libc::SO_ACCEPTCONN,
      &mut accept_val as *mut _ as *mut libc::c_void,
      &mut len,
    );
    assert_ne!(res, -1);
    assert_eq!(
      accept_val,
      1,
      "Socket should be in listening state {:?}",
      std::io::Error::last_os_error()
    );
    libc::close(sock.as_fd().as_raw_fd());
  }
}

#[cfg(linux)]
#[test]
fn test_listen_with_backlog() {
  let mut lio = Lio::new(64).unwrap();

  let (sender_sock, receiver_sock) = mpsc::channel();
  let (sender_unit, receiver_unit) = mpsc::channel::<std::io::Result<()>>();

  lio::test_utils::tcp_socket()
    .with_lio(&mut lio)
    .send_with(sender_sock.clone());

  let sock = poll_until_recv(&mut lio, &receiver_sock).expect("Failed to create socket");

  let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();

  bind(&sock, addr)
    .with_lio(&mut lio)
    .send_with(sender_unit.clone());

  poll_until_recv(&mut lio, &receiver_unit).expect("Failed to bind socket");

  // Listen with custom backlog
  listen(&sock, 10)
    .with_lio(&mut lio)
    .send_with(sender_unit.clone());

  poll_until_recv(&mut lio, &receiver_unit).expect("Failed to listen with backlog 10");

  // Verify listening state
  unsafe {
    let mut accept_val: i32 = 0;
    let mut len = std::mem::size_of::<i32>() as libc::socklen_t;
    libc::getsockopt(
      sock.as_fd().as_raw_fd(),
      libc::SOL_SOCKET,
      libc::SO_ACCEPTCONN,
      &mut accept_val as *mut _ as *mut libc::c_void,
      &mut len,
    );
    assert_eq!(accept_val, 1);
    libc::close(sock.as_fd().as_raw_fd());
  }
}

#[cfg(linux)]
#[test]
fn test_listen_large_backlog() {
  let mut lio = Lio::new(64).unwrap();

  let (sender_sock, receiver_sock) = mpsc::channel();
  let (sender_unit, receiver_unit) = mpsc::channel::<std::io::Result<()>>();

  lio::test_utils::tcp_socket()
    .with_lio(&mut lio)
    .send_with(sender_sock.clone());

  let sock = poll_until_recv(&mut lio, &receiver_sock).expect("Failed to create socket");

  let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();

  bind(&sock, addr)
    .with_lio(&mut lio)
    .send_with(sender_unit.clone());

  poll_until_recv(&mut lio, &receiver_unit).expect("Failed to bind socket");

  // Listen with large backlog
  listen(&sock, 1024)
    .with_lio(&mut lio)
    .send_with(sender_unit.clone());

  poll_until_recv(&mut lio, &receiver_unit).expect("Failed to listen with large backlog");

  // Verify listening state
  unsafe {
    let mut accept_val: i32 = 0;
    let mut len = std::mem::size_of::<i32>() as libc::socklen_t;
    libc::getsockopt(
      sock.as_fd().as_raw_fd(),
      libc::SOL_SOCKET,
      libc::SO_ACCEPTCONN,
      &mut accept_val as *mut _ as *mut libc::c_void,
      &mut len,
    );
    assert_eq!(accept_val, 1);
    libc::close(sock.as_fd().as_raw_fd());
  }
}

#[test]
fn test_listen_without_bind() {
  let mut lio = Lio::new(64).unwrap();

  let (sender_sock, receiver_sock) = mpsc::channel();

  lio::test_utils::tcp_socket()
    .with_lio(&mut lio)
    .send_with(sender_sock.clone());

  let sock = poll_until_recv(&mut lio, &receiver_sock).expect("Failed to create socket");

  let (sender_l, receiver_l) = mpsc::channel::<std::io::Result<()>>();

  // Try to listen without binding first
  api::listen(&sock, 128)
    .with_lio(&mut lio)
    .send_with(sender_l.clone());

  let _ = poll_until_recv(&mut lio, &receiver_l);

  // On most systems this will succeed (bind to INADDR_ANY:0)
  // but behavior may vary by platform
  unsafe {
    libc::close(sock.as_fd().as_raw_fd());
  }
}

#[cfg(linux)]
#[test]
fn test_listen_ipv6() {
  let mut lio = Lio::new(64).unwrap();

  let (sender_sock, receiver_sock) = mpsc::channel();
  let (sender_unit, receiver_unit) = mpsc::channel::<std::io::Result<()>>();

  lio::test_utils::tcp6_socket()
    .with_lio(&mut lio)
    .send_with(sender_sock.clone());

  let sock = poll_until_recv(&mut lio, &receiver_sock).expect("Failed to create IPv6 socket");

  let addr: SocketAddr = "[::1]:0".parse().unwrap();

  bind(&sock, addr)
    .with_lio(&mut lio)
    .send_with(sender_unit.clone());

  poll_until_recv(&mut lio, &receiver_unit).expect("Failed to bind IPv6 socket");

  listen(&sock, 128)
    .with_lio(&mut lio)
    .send_with(sender_unit.clone());

  poll_until_recv(&mut lio, &receiver_unit).expect("Failed to listen on IPv6 socket");

  // Verify listening state
  unsafe {
    let mut accept_val: i32 = 0;
    let mut len = std::mem::size_of::<i32>() as libc::socklen_t;
    libc::getsockopt(
      sock.as_fd().as_raw_fd(),
      libc::SOL_SOCKET,
      libc::SO_ACCEPTCONN,
      &mut accept_val as *mut _ as *mut libc::c_void,
      &mut len,
    );
    assert_eq!(accept_val, 1);
    libc::close(sock.as_fd().as_raw_fd());
  }
}

#[test]
fn test_listen_on_udp() {
  let mut lio = Lio::new(64).unwrap();

  let (sender_sock, receiver_sock) = mpsc::channel();
  let (sender_unit, receiver_unit) = mpsc::channel::<std::io::Result<()>>();

  lio::test_utils::udp_socket()
    .with_lio(&mut lio)
    .send_with(sender_sock.clone());

  let sock = poll_until_recv(&mut lio, &receiver_sock).expect("Failed to create UDP socket");

  let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();

  api::bind(&sock, addr)
    .with_lio(&mut lio)
    .send_with(sender_unit.clone());

  poll_until_recv(&mut lio, &receiver_unit).expect("Failed to bind UDP socket");

  let (sender_l, receiver_l) = mpsc::channel::<std::io::Result<()>>();

  // Try to listen on UDP socket (should fail)
  api::listen(&sock, 128)
    .with_lio(&mut lio)
    .send_with(sender_l.clone());

  let result = poll_until_recv(&mut lio, &receiver_l);

  assert!(result.is_err(), "Listen should fail on UDP socket");

  // Cleanup
  unsafe {
    libc::close(sock.as_fd().as_raw_fd());
  }
}

#[test]
fn test_listen_twice() {
  let mut lio = Lio::new(64).unwrap();

  let (sender_sock, receiver_sock) = mpsc::channel();
  let (sender_unit, receiver_unit) = mpsc::channel::<std::io::Result<()>>();

  lio::test_utils::tcp_socket()
    .with_lio(&mut lio)
    .send_with(sender_sock.clone());

  let sock = poll_until_recv(&mut lio, &receiver_sock).expect("Failed to create socket");

  let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();

  api::bind(&sock, addr)
    .with_lio(&mut lio)
    .send_with(sender_unit.clone());

  poll_until_recv(&mut lio, &receiver_unit).expect("Failed to bind socket");

  api::listen(&sock, 128)
    .with_lio(&mut lio)
    .send_with(sender_unit.clone());

  poll_until_recv(&mut lio, &receiver_unit).expect("First listen should succeed");

  let (sender_l2, receiver_l2) = mpsc::channel::<std::io::Result<()>>();

  // Try to listen again on the same socket
  api::listen(&sock, 256)
    .with_lio(&mut lio)
    .send_with(sender_l2.clone());

  let _ = poll_until_recv(&mut lio, &receiver_l2);

  // Behavior may vary - some systems allow it, some don't
  unsafe {
    libc::close(sock.as_fd().as_raw_fd());
  }
}

#[cfg(linux)]
#[test]
fn test_listen_zero_backlog() {
  let mut lio = Lio::new(64).unwrap();

  let (sender_sock, receiver_sock) = mpsc::channel();
  let (sender_unit, receiver_unit) = mpsc::channel::<std::io::Result<()>>();

  lio::test_utils::tcp_socket()
    .with_lio(&mut lio)
    .send_with(sender_sock.clone());

  let sock = poll_until_recv(&mut lio, &receiver_sock).expect("Failed to create socket");

  let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();

  api::bind(&sock, addr)
    .with_lio(&mut lio)
    .send_with(sender_unit.clone());

  poll_until_recv(&mut lio, &receiver_unit).expect("Failed to bind socket");

  // Listen with backlog of 0 (system may adjust to minimum)
  api::listen(&sock, 0)
    .with_lio(&mut lio)
    .send_with(sender_unit.clone());

  poll_until_recv(&mut lio, &receiver_unit).expect("Failed to listen with backlog 0");

  // Verify listening state
  unsafe {
    let mut accept_val: i32 = 0;
    let mut len = std::mem::size_of::<i32>() as libc::socklen_t;
    libc::getsockopt(
      sock.as_fd().as_raw_fd(),
      libc::SOL_SOCKET,
      libc::SO_ACCEPTCONN,
      &mut accept_val as *mut _ as *mut libc::c_void,
      &mut len,
    );
    assert_eq!(accept_val, 1);
    libc::close(sock.as_fd().as_raw_fd());
  }
}

#[test]
fn test_listen_after_close() {
  let mut lio = Lio::new(64).unwrap();

  let (sender_sock, receiver_sock) = mpsc::channel();

  lio::test_utils::tcp_socket()
    .with_lio(&mut lio)
    .send_with(sender_sock.clone());

  let sock = poll_until_recv(&mut lio, &receiver_sock).expect("Failed to create socket");

  let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();

  let (sender_bind, receiver_bind) = mpsc::channel();
  api::bind(&sock, addr)
    .with_lio(&mut lio)
    .send_with(sender_bind);

  poll_until_recv(&mut lio, &receiver_bind).expect("Failed to bind socket");

  unsafe {
    libc::close(sock.as_fd().as_raw_fd());
  }

  // Try to listen on closed socket
  let (sender_l, receiver_l) = mpsc::channel();
  api::listen(&sock, 128)
    .with_lio(&mut lio)
    .send_with(sender_l);

  let result = poll_until_recv(&mut lio, &receiver_l);

  assert!(result.is_err(), "Listen should fail on closed socket");
}

#[cfg(linux)]
#[test]
fn test_listen_concurrent() {
  let mut lio = Lio::new(64).unwrap();

  let (sender, receiver) = mpsc::channel();

  // Test listening on multiple sockets concurrently
  for _ in 0..10 {
    lio::test_utils::tcp_socket()
      .with_lio(&mut lio)
      .send_with(sender.clone());
  }

  let mut sockets = Vec::new();
  for _ in 0..10 {
    let sock = poll_until_recv(&mut lio, &receiver).expect("Socket creation failed");
    sockets.push(sock);
  }

  let (sender_bind, receiver_bind) = mpsc::channel::<std::io::Result<()>>();

  for sock in &sockets {
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    api::bind(&sock, addr)
      .with_lio(&mut lio)
      .send_with(sender_bind.clone());
  }

  for _ in 0..10 {
    poll_until_recv(&mut lio, &receiver_bind).expect("Failed to bind socket");
  }

  let (sender_listen, receiver_listen) = mpsc::channel::<std::io::Result<()>>();

  for sock in &sockets {
    api::listen(sock, 128)
      .with_lio(&mut lio)
      .send_with(sender_listen.clone());
  }

  for _ in 0..10 {
    poll_until_recv(&mut lio, &receiver_listen).expect("Failed to listen");
  }

  for sock in sockets {
    unsafe {
      let mut accept_val: i32 = 0;
      let mut len = std::mem::size_of::<i32>() as libc::socklen_t;
      libc::getsockopt(
        sock.as_raw_fd(),
        libc::SOL_SOCKET,
        libc::SO_ACCEPTCONN,
        &mut accept_val as *mut _ as *mut libc::c_void,
        &mut len,
      );
      assert_eq!(accept_val, 1);
      libc::close(sock.as_fd().as_raw_fd());
    }
  }
}

#[cfg(linux)]
#[test]
fn test_listen_on_all_interfaces() {
  let mut lio = Lio::new(64).unwrap();

  let (sender_sock, receiver_sock) = mpsc::channel();
  let (sender_unit, receiver_unit) = mpsc::channel::<std::io::Result<()>>();

  lio::test_utils::tcp_socket()
    .with_lio(&mut lio)
    .send_with(sender_sock.clone());

  let sock = poll_until_recv(&mut lio, &receiver_sock).expect("Failed to create socket");

  // Bind to 0.0.0.0 (all interfaces)
  let addr: SocketAddr = "0.0.0.0:0".parse().unwrap();

  api::bind(&sock, addr)
    .with_lio(&mut lio)
    .send_with(sender_unit.clone());

  poll_until_recv(&mut lio, &receiver_unit).expect("Failed to bind to all interfaces");

  listen(&sock, 128)
    .with_lio(&mut lio)
    .send_with(sender_unit.clone());

  poll_until_recv(&mut lio, &receiver_unit).expect("Failed to listen on all interfaces");

  // Verify listening state
  unsafe {
    let mut accept_val: i32 = 0;
    let mut len = std::mem::size_of::<i32>() as libc::socklen_t;
    libc::getsockopt(
      sock.as_fd().as_raw_fd(),
      libc::SOL_SOCKET,
      libc::SO_ACCEPTCONN,
      &mut accept_val as *mut _ as *mut libc::c_void,
      &mut len,
    );
    assert_eq!(accept_val, 1);
    libc::close(sock.as_fd().as_raw_fd());
  }
}
