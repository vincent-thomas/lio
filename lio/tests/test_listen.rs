use lio::{bind, listen};
use std::{net::SocketAddr, os::fd::{AsFd, AsRawFd}, sync::mpsc};

#[cfg(linux)]
#[ignore = "flaky network test"]
#[test]
fn test_listen_basic() {
  lio::init();

  let (sender_sock, receiver_sock) = mpsc::channel();
  let (sender_unit, receiver_unit) = mpsc::channel();

  let sender_s1 = sender_sock.clone();
  lio::test_utils::tcp_socket().when_done(
    move |res| {
      sender_s1.send(res).unwrap();
    },
  );

  lio::tick();

  let sock = receiver_sock.recv().unwrap().expect("Failed to create socket");

  let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();

  let sender_b = sender_unit.clone();
  bind(&sock, addr).when_done(move |res| {
    sender_b.send(res).unwrap();
  });

  lio::tick();

  receiver_unit.recv().unwrap().expect("Failed to bind socket");

  let sender_l = sender_unit.clone();
  listen(&sock,128).when_done(move |res| {
    sender_l.send(res).unwrap();
  });

  lio::tick();

  receiver_unit.recv().unwrap().expect("Failed to listen on socket");

  // Verify socket is in listening state by checking it accepts connections
  unsafe {
    let mut accept_val: i32 = 0;
    let mut len = std::mem::size_of::<i32>() as libc::socklen_t;
    let res = libc::getsockopt(
      sock,
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
#[ignore = "flaky network test"]
#[test]
fn test_listen_with_backlog() {
  lio::init();

  let (sender_sock, receiver_sock) = mpsc::channel();
  let (sender_unit, receiver_unit) = mpsc::channel();

  let sender_s1 = sender_sock.clone();
  lio::test_utils::tcp_socket().when_done(
    move |res| {
      sender_s1.send(res).unwrap();
    },
  );

  lio::tick();

  let sock = receiver_sock.recv().unwrap().expect("Failed to create socket");

  let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();

  let sender_b = sender_unit.clone();
  bind(&sock, addr).when_done(move |res| {
    sender_b.send(res).unwrap();
  });

  lio::tick();

  receiver_unit.recv().unwrap().expect("Failed to bind socket");

  // Listen with custom backlog
  let sender_l = sender_unit.clone();
  listen(&sock,10).when_done(move |res| {
    sender_l.send(res).unwrap();
  });

  lio::tick();

  receiver_unit.recv().unwrap().expect("Failed to listen with backlog 10");

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
#[ignore = "flaky network test"]
#[test]
fn test_listen_large_backlog() {
  lio::init();

  let (sender_sock, receiver_sock) = mpsc::channel();
  let (sender_unit, receiver_unit) = mpsc::channel();

  let sender_s1 = sender_sock.clone();
  lio::test_utils::tcp_socket().when_done(
    move |res| {
      sender_s1.send(res).unwrap();
    },
  );

  lio::tick();

  let sock = receiver_sock.recv().unwrap().expect("Failed to create socket");

  let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();

  let sender_b = sender_unit.clone();
  bind(&sock, addr).when_done(move |res| {
    sender_b.send(res).unwrap();
  });

  lio::tick();

  receiver_unit.recv().unwrap().expect("Failed to bind socket");

  // Listen with large backlog
  let sender_l = sender_unit.clone();
  listen(&sock,1024).when_done(move |res| {
    sender_l.send(res).unwrap();
  });

  lio::tick();

  receiver_unit.recv().unwrap().expect("Failed to listen with large backlog");

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

#[ignore = "flaky network test"]
#[test]
fn test_listen_without_bind() {
  lio::init();

  let (sender_sock, receiver_sock) = mpsc::channel();

  let sender_s1 = sender_sock.clone();
  lio::test_utils::tcp_socket().when_done(
    move |res| {
      sender_s1.send(res).unwrap();
    },
  );

  lio::tick();

  let sock = receiver_sock.recv().unwrap().expect("Failed to create socket");

  let (sender_l, receiver_l) = mpsc::channel();
  let sender_l1 = sender_l.clone();

  // Try to listen without binding first
  listen(&sock, 128).when_done(move |res| {
    sender_l1.send(res).unwrap();
  });

  lio::tick();

  let _ = receiver_l.recv().unwrap();

  // On most systems this will succeed (bind to INADDR_ANY:0)
  // but behavior may vary by platform
  unsafe {
    libc::close(sock.as_fd().as_raw_fd());
  }
}

#[cfg(linux)]
#[ignore = "flaky network test"]
#[test]
fn test_listen_ipv6() {
  lio::init();

  let (sender_sock, receiver_sock) = mpsc::channel();
  let (sender_unit, receiver_unit) = mpsc::channel();

  let sender_s1 = sender_sock.clone();
  lio::test_utils::tcp6_socket().when_done(
    move |res| {
      sender_s1.send(res).unwrap();
    },
  );

  lio::tick();

  let sock =
    receiver_sock.recv().unwrap().expect("Failed to create IPv6 socket");

  let addr: SocketAddr = "[::1]:0".parse().unwrap();

  let sender_b = sender_unit.clone();
  bind(&sock, addr).when_done(move |res| {
    sender_b.send(res).unwrap();
  });

  lio::tick();

  receiver_unit.recv().unwrap().expect("Failed to bind IPv6 socket");

  let sender_l = sender_unit.clone();
  listen(&sock,128).when_done(move |res| {
    sender_l.send(res).unwrap();
  });

  lio::tick();

  receiver_unit.recv().unwrap().expect("Failed to listen on IPv6 socket");

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

#[ignore = "flaky network test"]
#[test]
fn test_listen_on_udp() {
  lio::init();

  let (sender_sock, receiver_sock) = mpsc::channel();
  let (sender_unit, receiver_unit) = mpsc::channel();

  let sender_s1 = sender_sock.clone();
  lio::test_utils::udp_socket().when_done(
    move |res| {
      sender_s1.send(res).unwrap();
    },
  );

  lio::tick();

  let sock =
    receiver_sock.recv().unwrap().expect("Failed to create UDP socket");

  let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();

  let sender_b = sender_unit.clone();
  bind(&sock, addr).when_done(move |res| {
    sender_b.send(res).unwrap();
  });

  lio::tick();

  receiver_unit.recv().unwrap().expect("Failed to bind UDP socket");

  let (sender_l, receiver_l) = mpsc::channel();
  let sender_l1 = sender_l.clone();

  // Try to listen on UDP socket (should fail)
  listen(&sock,128).when_done(move |res| {
    sender_l1.send(res).unwrap();
  });

  lio::tick();

  let result = receiver_l.recv().unwrap();

  assert!(result.is_err(), "Listen should fail on UDP socket");

  // Cleanup
  unsafe {
    libc::close(sock.as_fd().as_raw_fd());
  }
}

#[ignore = "flaky network test"]
#[test]
fn test_listen_twice() {
  lio::init();

  let (sender_sock, receiver_sock) = mpsc::channel();
  let (sender_unit, receiver_unit) = mpsc::channel();

  let sender_s1 = sender_sock.clone();
  lio::test_utils::tcp_socket().when_done(
    move |res| {
      sender_s1.send(res).unwrap();
    },
  );

  lio::tick();

  let sock = receiver_sock.recv().unwrap().expect("Failed to create socket");

  let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();

  let sender_b = sender_unit.clone();
  bind(&sock, addr).when_done(move |res| {
    sender_b.send(res).unwrap();
  });

  lio::tick();

  receiver_unit.recv().unwrap().expect("Failed to bind socket");

  let sender_l = sender_unit.clone();
  listen(&sock,128).when_done(move |res| {
    sender_l.send(res).unwrap();
  });

  lio::tick();

  receiver_unit.recv().unwrap().expect("First listen should succeed");

  let (sender_l2, receiver_l2) = mpsc::channel();
  let sender_l3 = sender_l2.clone();

  // Try to listen again on the same socket
  listen(&sock,256).when_done(move |res| {
    sender_l3.send(res).unwrap();
  });

  lio::tick();

  let _ = receiver_l2.recv().unwrap();

  // Behavior may vary - some systems allow it, some don't
  unsafe {
    libc::close(sock.as_fd().as_raw_fd());
  }
}

#[cfg(linux)]
#[ignore = "flaky network test"]
#[test]
fn test_listen_zero_backlog() {
  lio::init();

  let (sender_sock, receiver_sock) = mpsc::channel();
  let (sender_unit, receiver_unit) = mpsc::channel();

  let sender_s1 = sender_sock.clone();
  lio::test_utils::tcp_socket().when_done(
    move |res| {
      sender_s1.send(res).unwrap();
    },
  );

  lio::tick();

  let sock = receiver_sock.recv().unwrap().expect("Failed to create socket");

  let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();

  let sender_b = sender_unit.clone();
  bind(&sock, addr).when_done(move |res| {
    sender_b.send(res).unwrap();
  });

  lio::tick();

  receiver_unit.recv().unwrap().expect("Failed to bind socket");

  // Listen with backlog of 0 (system may adjust to minimum)
  let sender_l = sender_unit.clone();
  listen(&sock,0).when_done(move |res| {
    sender_l.send(res).unwrap();
  });

  lio::tick();

  receiver_unit.recv().unwrap().expect("Failed to listen with backlog 0");

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

#[ignore = "flaky network test"]
#[test]
fn test_listen_after_close() {
  lio::init();

  let mut socket_recv =
    lio::test_utils::tcp_socket().send();

  lio::tick();

  let sock = socket_recv.try_recv().unwrap().expect("Failed to create socket");

  let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();

  let mut bind_recv = bind(&sock, addr).send();

  lio::tick();

  bind_recv.try_recv().unwrap().expect("Failed to bind socket");

  unsafe {
    libc::close(sock.as_fd().as_raw_fd());
  }

  // Try to listen on closed socket
  let mut listen_recv = listen(&sock, 128).send();

  lio::tick();

  let result = listen_recv.try_recv().unwrap();

  assert!(result.is_err(), "Listen should fail on closed socket");
}

#[cfg(linux)]
#[ignore = "flaky network test"]
#[test]
fn test_listen_concurrent() {
  lio::init();

  let (sender, receiver) = mpsc::channel();

  // Test listening on multiple sockets concurrently
  for _ in 0..10 {
    let sender_sock = sender.clone();
    lio::test_utils::tcp_socket().when_done(
      move |res| {
        sender_sock.send(res).unwrap();
      },
    );
  }

  lio::tick();

  let mut sockets = Vec::new();
  for _ in 0..10 {
    let sock = receiver.recv().unwrap().expect("Failed to create socket");
    sockets.push(sock);
  }

  let (sender_bind, receiver_bind) = mpsc::channel();

  for &sock in &sockets {
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let sender_b = sender_bind.clone();
    bind(&sock, addr).when_done(move |res| {
      sender_b.send(res).unwrap();
    });
  }

  lio::tick();

  for _ in 0..10 {
    receiver_bind.recv().unwrap().expect("Failed to bind socket");
  }

  let (sender_listen, receiver_listen) = mpsc::channel();

  for &sock in &sockets {
    let sender_l = sender_listen.clone();
    listen(&sock,128).when_done(move |res| {
      sender_l.send(res).unwrap();
    });
  }

  lio::tick();

  for _ in 0..10 {
    receiver_listen.recv().unwrap().expect("Failed to listen");
  }

  for sock in sockets {
    unsafe {
      let mut accept_val: i32 = 0;
      let mut len = std::mem::size_of::<i32>() as libc::socklen_t;
      libc::getsockopt(
        sock,
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
#[ignore = "flaky network test"]
#[test]
fn test_listen_on_all_interfaces() {
  lio::init();

  let (sender_sock, receiver_sock) = mpsc::channel();
  let (sender_unit, receiver_unit) = mpsc::channel();

  let sender_s1 = sender_sock.clone();
  lio::test_utils::tcp_socket().when_done(
    move |res| {
      sender_s1.send(res).unwrap();
    },
  );

  lio::tick();

  let sock = receiver_sock.recv().unwrap().expect("Failed to create socket");

  // Bind to 0.0.0.0 (all interfaces)
  let addr: SocketAddr = "0.0.0.0:0".parse().unwrap();

  let sender_b = sender_unit.clone();
  bind(&sock, addr).when_done(move |res| {
    sender_b.send(res).unwrap();
  });

  lio::tick();

  receiver_unit.recv().unwrap().expect("Failed to bind to all interfaces");

  let sender_l = sender_unit.clone();
  listen(&sock,128).when_done(move |res| {
    sender_l.send(res).unwrap();
  });

  lio::tick();

  receiver_unit.recv().unwrap().expect("Failed to listen on all interfaces");

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
