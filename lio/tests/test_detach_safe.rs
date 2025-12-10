mod common;

use lio::*;
use std::ffi::CString;
use std::sync::mpsc::{self, TryRecvError, sync_channel};
use std::sync::{Arc, Mutex};

// ============================================================================
// DETACH SAFE OPERATIONS - Can use .detach()
// ============================================================================

/// Test Close (DetachSafe) with .detach()
#[test]
fn test_close_detach_safe() {
  lio::init();

  let mut fds = [0i32; 2];
  unsafe {
    assert_eq!(libc::pipe(fds.as_mut_ptr()), 0);
  }
  let fd = fds[0];

  // DetachSafe: can use .detach()
  close(fd).detach();

  lio::tick();

  // Verify fd is closed
  let result = unsafe { libc::close(fd) };
  assert_eq!(result, -1, "fd should already be closed");

  unsafe {
    libc::close(fds[1]);
  }
}

/// Test Close (DetachSafe) with .when_done()
#[test]
fn test_close_when_done() {
  lio::init();

  let mut fds = [0i32; 2];
  unsafe {
    libc::pipe(fds.as_mut_ptr());
  }
  let fd = fds[0];

  let (sender, receiver) = mpsc::channel();
  let sender1 = sender.clone();

  close(fd).when_done(move |result| {
    assert!(result.is_ok());
    sender1.send(()).unwrap();
  });

  // assert_eq!(receiver.try_recv().unwrap_err(), TryRecvError::Empty);

  lio::tick();

  receiver.recv().unwrap();

  unsafe {
    libc::close(fds[1]);
  }
}

/// Test Bind (DetachSafe) with .detach()
#[test]
fn test_bind_detach_safe() {
  lio::init();

  let sock = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
  assert!(sock >= 0);

  let addr = "127.0.0.1:0".parse().unwrap();
  bind(sock, addr).detach();

  lio::tick();

  unsafe {
    libc::close(sock);
  }
}

/// Test Bind (DetachSafe) with .when_done()
#[test]
fn test_bind_when_done() {
  lio::init();

  let sock = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
  let addr = "127.0.0.1:0".parse().unwrap();

  let (sender, receiver) = mpsc::channel();
  let sender1 = sender.clone();

  bind(sock, addr).when_done(move |result| {
    assert!(result.is_ok());
    sender1.send(()).unwrap();
  });

  // assert_eq!(receiver.try_recv().unwrap_err(), TryRecvError::Empty);

  lio::tick();

  receiver.recv().unwrap();

  unsafe {
    libc::close(sock);
  }
}

/// Test Connect (DetachSafe) with .detach()
#[test]
fn test_connect_detach_safe() {
  lio::init();

  let listen_sock =
    unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
  let addr = "127.0.0.1:0".parse().unwrap();

  let (sender, receiver) = mpsc::channel();
  let sender1 = sender.clone();
  let sender2 = sender.clone();

  bind(listen_sock, addr).when_done(move |result| {
    result.unwrap();
    sender1.send(()).unwrap();
  });

  // assert_eq!(receiver.try_recv().unwrap_err(), TryRecvError::Empty);

  lio::tick();

  receiver.recv().unwrap();

  listen(listen_sock, 5).when_done(move |result| {
    result.unwrap();
    sender2.send(()).unwrap();
  });

  // assert_eq!(receiver.try_recv().unwrap_err(), TryRecvError::Empty);

  lio::tick();

  receiver.recv().unwrap();

  let mut sockaddr: libc::sockaddr_in = unsafe { std::mem::zeroed() };
  let mut addrlen = std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
  unsafe {
    libc::getsockname(
      listen_sock,
      &mut sockaddr as *mut _ as *mut libc::sockaddr,
      &mut addrlen,
    );
  }
  let port = u16::from_be(sockaddr.sin_port);
  let connect_addr = format!("127.0.0.1:{}", port).parse().unwrap();

  let client_sock =
    unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };

  connect(client_sock, connect_addr).detach();

  lio::tick();

  unsafe {
    libc::close(client_sock);
    libc::close(listen_sock);
  }
}

/// Test Connect (DetachSafe) with .when_done()
#[test]
fn test_connect_when_done() {
  lio::init();

  let listen_sock =
    unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
  let addr = "127.0.0.1:0".parse().unwrap();

  let (sender, receiver) = mpsc::channel();
  let sender1 = sender.clone();
  let sender2 = sender.clone();
  let sender3 = sender.clone();

  bind(listen_sock, addr).when_done(move |result| {
    result.unwrap();
    sender1.send(()).unwrap();
  });

  // assert_eq!(receiver.try_recv().unwrap_err(), TryRecvError::Empty);

  lio::tick();

  receiver.recv().unwrap();

  listen(listen_sock, 5).when_done(move |result| {
    result.unwrap();
    sender2.send(()).unwrap();
  });

  // assert_eq!(receiver.try_recv().unwrap_err(), TryRecvError::Empty);

  lio::tick();

  receiver.recv().unwrap();

  let mut sockaddr: libc::sockaddr_in = unsafe { std::mem::zeroed() };
  let mut addrlen = std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
  unsafe {
    libc::getsockname(
      listen_sock,
      &mut sockaddr as *mut _ as *mut libc::sockaddr,
      &mut addrlen,
    );
  }
  let port = u16::from_be(sockaddr.sin_port);
  let connect_addr = format!("127.0.0.1:{}", port).parse().unwrap();

  let client_sock =
    unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };

  // Connect with .when_done() - DetachSafe means it CAN use .detach()
  connect(client_sock, connect_addr).when_done(move |result| {
    result.unwrap();
    sender3.send(()).unwrap();
  });

  // assert_eq!(receiver.try_recv().unwrap_err(), TryRecvError::Empty);

  lio::tick();

  receiver.recv().unwrap();

  unsafe {
    libc::close(client_sock);
    libc::close(listen_sock);
  }
}

/// Test Fsync (DetachSafe) with .detach()
#[test]
fn test_fsync_detach_safe() {
  lio::init();

  let path = CString::new("/tmp/lio_test_fsync_detach.txt").unwrap();
  let fd = unsafe {
    libc::open(
      path.as_ptr(),
      libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
      0o644,
    )
  };

  let data = b"test data";
  unsafe {
    libc::write(fd, data.as_ptr() as *const libc::c_void, data.len());
  }

  fsync(fd).detach();

  lio::tick();

  unsafe {
    libc::close(fd);
    libc::unlink(path.as_ptr());
  }
}

/// Test Fsync (DetachSafe) with .when_done()
#[test]
fn test_fsync_when_done() {
  lio::init();

  let path = CString::new("/tmp/lio_test_fsync_when_done.txt").unwrap();
  let fd = unsafe {
    libc::open(
      path.as_ptr(),
      libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
      0o644,
    )
  };

  let data = b"test data";
  unsafe {
    libc::write(fd, data.as_ptr() as *const libc::c_void, data.len());
  }

  let (sender, receiver) = mpsc::channel();
  let sender1 = sender.clone();

  fsync(fd).when_done(move |result| {
    assert!(result.is_ok());
    sender1.send(()).unwrap();
  });

  // assert_eq!(receiver.try_recv().unwrap_err(), TryRecvError::Empty);

  lio::tick();

  receiver.recv().unwrap();

  unsafe {
    libc::close(fd);
    libc::unlink(path.as_ptr());
  }
}

// ============================================================================
// NON-DETACH SAFE OPERATIONS - Must use .when_done() or .await, NOT .detach()
// ============================================================================

/// Test Read (NOT DetachSafe) with .when_done()
#[test]
fn test_read_when_done_not_detach_safe() {
  lio::init();

  let path = CString::new("/tmp/lio_test_read_when_done.txt").unwrap();
  let fd = unsafe {
    let fd = libc::open(
      path.as_ptr(),
      libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
      0o644,
    );
    let data = b"hello";
    libc::write(fd, data.as_ptr() as *const libc::c_void, data.len());
    fd
  };

  let (sender, receiver) = mpsc::channel();
  let sender1 = sender.clone();

  read(fd, vec![0u8; 100], 0).when_done(move |(result, buf)| {
    let bytes_read = result.expect("Read should succeed");
    assert_eq!(bytes_read, 5);
    assert_eq!(&buf[..5], b"hello");
    sender1.send(()).unwrap();
  });

  // assert_eq!(receiver.try_recv().unwrap_err(), TryRecvError::Empty);

  lio::tick();

  receiver.recv().unwrap();

  unsafe {
    libc::close(fd);
    libc::unlink(path.as_ptr());
  }
}

/// Test Write (NOT DetachSafe) with .when_done()
#[test]
fn test_write_when_done_not_detach_safe() {
  lio::init();

  let path = CString::new("/tmp/lio_test_write_when_done.txt").unwrap();
  let fd = unsafe {
    libc::open(
      path.as_ptr(),
      libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
      0o644,
    )
  };

  let (sender, receiver) = mpsc::channel();
  let sender1 = sender.clone();

  write(fd, b"test data".to_vec(), 0).when_done(move |(result, _buf)| {
    let bytes_written = result.expect("Write should succeed");
    assert_eq!(bytes_written, 9);
    sender1.send(()).unwrap();
  });

  // assert_eq!(receiver.try_recv().unwrap_err(), TryRecvError::Empty);

  lio::tick();

  receiver.recv().unwrap();

  unsafe {
    libc::close(fd);
    libc::unlink(path.as_ptr());
  }
}

/// Test Accept (NOT DetachSafe) with .when_done()
#[test]
fn test_accept_with_when_done_not_detach_safe() {
  lio::init();

  let server_sock =
    unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
  let addr = "127.0.0.1:0".parse().unwrap();

  let (sender, receiver) = mpsc::channel();
  let sender1 = sender.clone();
  let sender2 = sender.clone();
  let sender3 = sender.clone();

  bind(server_sock, addr).when_done(move |result| {
    result.unwrap();
    sender1.send(()).unwrap();
  });

  // assert_eq!(receiver.try_recv().unwrap_err(), TryRecvError::Empty);

  lio::tick();

  receiver.recv().unwrap();

  listen(server_sock, 5).when_done(move |result| {
    result.unwrap();
    sender2.send(()).unwrap();
  });

  // assert_eq!(receiver.try_recv().unwrap_err(), TryRecvError::Empty);

  lio::tick();

  receiver.recv().unwrap();

  let mut sockaddr: libc::sockaddr_in = unsafe { std::mem::zeroed() };
  let mut addrlen = std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
  unsafe {
    libc::getsockname(
      server_sock,
      &mut sockaddr as *mut _ as *mut libc::sockaddr,
      &mut addrlen,
    );
  }
  let port = u16::from_be(sockaddr.sin_port);
  let connect_addr = format!("127.0.0.1:{}", port).parse().unwrap();

  // Connect from client
  let client_sock =
    unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };

  connect(client_sock, connect_addr).when_done(move |result| {
    result.unwrap();
    sender3.send(()).unwrap();
  });

  // assert_eq!(receiver.try_recv().unwrap_err(), TryRecvError::Empty);

  lio::tick();

  receiver.recv().unwrap();

  let (sender4, receiver4) = mpsc::channel();
  let sender5 = sender4.clone();

  // Accept with .when_done()
  accept(server_sock).when_done(move |result| {
    let (accepted_fd, _) = result.unwrap();
    sender5.send(accepted_fd).unwrap();
  });

  // assert_eq!(receiver4.try_recv().unwrap_err(), TryRecvError::Empty);

  lio::tick();

  let accepted_fd = receiver4.recv().unwrap();

  unsafe {
    libc::close(accepted_fd);
    libc::close(client_sock);
    libc::close(server_sock);
  }
}

/// Test Listen (NOT DetachSafe) with .when_done()
#[test]
fn test_listen_when_done_not_detach_safe() {
  lio::init();

  let sock = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
  let addr = "127.0.0.1:0".parse().unwrap();

  let (sender, receiver) = mpsc::channel();
  let sender1 = sender.clone();
  let sender2 = sender.clone();

  bind(sock, addr).when_done(move |result| {
    result.unwrap();
    sender1.send(()).unwrap();
  });

  // assert_eq!(receiver.try_recv().unwrap_err(), TryRecvError::Empty);

  lio::tick();

  receiver.recv().unwrap();

  listen(sock, 5).when_done(move |result| {
    assert!(result.is_ok());
    sender2.send(()).unwrap();
  });

  // assert_eq!(receiver.try_recv().unwrap_err(), TryRecvError::Empty);

  lio::tick();

  receiver.recv().unwrap();

  unsafe {
    libc::close(sock);
  }
}

/// Test Recv (NOT DetachSafe) with .when_done()
#[test]
fn test_recv_with_when_done_not_detach_safe() {
  lio::init();

  let server_sock =
    unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
  let addr = "127.0.0.1:0".parse().unwrap();

  let (sender, receiver) = mpsc::channel();
  let sender1 = sender.clone();
  let sender2 = sender.clone();
  let sender3 = sender.clone();
  let (accept_sender, accept_receiver) = mpsc::channel();
  // let sender4 = sender.clone();

  bind(server_sock, addr).when_done(move |result| {
    result.unwrap();
    sender1.send(()).unwrap();
  });

  // assert_eq!(receiver.try_recv().unwrap_err(), TryRecvError::Empty);

  lio::tick();

  receiver.recv().unwrap();

  listen(server_sock, 5).when_done(move |result| {
    result.unwrap();
    sender2.send(()).unwrap();
  });

  // assert_eq!(receiver.try_recv().unwrap_err(), TryRecvError::Empty);

  lio::tick();

  receiver.recv().unwrap();

  let mut sockaddr: libc::sockaddr_in = unsafe { std::mem::zeroed() };
  let mut addrlen = std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
  unsafe {
    libc::getsockname(
      server_sock,
      &mut sockaddr as *mut _ as *mut libc::sockaddr,
      &mut addrlen,
    );
  }
  let port = u16::from_be(sockaddr.sin_port);
  let connect_addr = format!("127.0.0.1:{}", port).parse().unwrap();

  let client_sock =
    unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };

  connect(client_sock, connect_addr).when_done(move |result| {
    result.unwrap();
    sender3.send(()).unwrap();
  });

  // assert_eq!(receiver.try_recv().unwrap_err(), TryRecvError::Empty);

  lio::tick();

  receiver.recv().unwrap();

  accept(server_sock).when_done(move |result| {
    let (accepted_fd, _) = result.unwrap();
    accept_sender.send(accepted_fd).unwrap();
  });

  // assert_eq!(receiver.try_recv().unwrap_err(), TryRecvError::Empty);

  lio::tick();

  let accepted_fd = accept_receiver.recv().unwrap();

  // Send data from client
  let data = b"hello";
  unsafe {
    libc::send(
      client_sock,
      data.as_ptr() as *const libc::c_void,
      data.len(),
      0,
    );
  }

  let (sender5, receiver5) = mpsc::channel();
  let sender6 = sender5.clone();

  // Recv with .when_done()
  recv(accepted_fd, vec![0u8; 100], None).when_done(move |(result, buf)| {
    let bytes_received = result.unwrap();
    assert_eq!(bytes_received, 5);
    assert_eq!(&buf[..5], b"hello");
    sender6.send(()).unwrap();
  });

  // assert_eq!(receiver5.try_recv().unwrap_err(), TryRecvError::Empty);

  lio::tick();

  receiver5.recv().unwrap();

  unsafe {
    libc::close(accepted_fd);
    libc::close(client_sock);
    libc::close(server_sock);
  }
}

/// Test Send (NOT DetachSafe) with .when_done()
#[test]
fn test_send_with_when_done_not_detach_safe() {
  lio::init();

  let server_sock =
    unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
  let addr = "127.0.0.1:0".parse().unwrap();

  let (sender, receiver) = mpsc::channel();
  let sender1 = sender.clone();
  let sender2 = sender.clone();
  let sender3 = sender.clone();

  bind(server_sock, addr).when_done(move |result| {
    result.unwrap();
    sender1.send(()).unwrap();
  });

  // assert_eq!(receiver.try_recv().unwrap_err(), TryRecvError::Empty);

  lio::tick();

  receiver.recv().unwrap();

  listen(server_sock, 5).when_done(move |result| {
    result.unwrap();
    sender2.send(()).unwrap();
  });

  // assert_eq!(receiver.try_recv().unwrap_err(), TryRecvError::Empty);

  lio::tick();

  receiver.recv().unwrap();

  let mut sockaddr: libc::sockaddr_in = unsafe { std::mem::zeroed() };
  let mut addrlen = std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
  unsafe {
    libc::getsockname(
      server_sock,
      &mut sockaddr as *mut _ as *mut libc::sockaddr,
      &mut addrlen,
    );
  }
  let port = u16::from_be(sockaddr.sin_port);
  let connect_addr = format!("127.0.0.1:{}", port).parse().unwrap();

  let client_sock =
    unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };

  connect(client_sock, connect_addr).when_done(move |result| {
    result.unwrap();
    sender3.send(()).unwrap();
  });

  // assert_eq!(receiver.try_recv().unwrap_err(), TryRecvError::Empty);

  lio::tick();

  receiver.recv().unwrap();

  let (sender4, receiver4) = mpsc::channel();
  let sender5 = sender4.clone();

  // Send with .when_done()
  send(client_sock, b"test".to_vec(), None).when_done(move |(result, _buf)| {
    let bytes_sent = result.unwrap();
    assert_eq!(bytes_sent, 4);
    sender5.send(()).unwrap();
  });

  // assert_eq!(receiver4.try_recv().unwrap_err(), TryRecvError::Empty);

  lio::tick();

  receiver4.recv().unwrap();

  unsafe {
    libc::close(client_sock);
    libc::close(server_sock);
  }
}

/// Test Socket (NOT DetachSafe) with .when_done()
#[test]
fn test_socket_when_done_not_detach_safe() {
  lio::init();

  let (sender, receiver) = mpsc::channel();
  let sender1 = sender.clone();

  socket(socket2::Domain::IPV4, socket2::Type::STREAM, None).when_done(
    move |result| {
      let fd = result.expect("Socket creation should succeed");
      unsafe {
        libc::close(fd);
      }
      sender1.send(()).unwrap();
    },
  );

  // assert_eq!(receiver.try_recv().unwrap_err(), TryRecvError::Empty);

  lio::tick();

  receiver.recv().unwrap();
}

/// Test OpenAt (NOT DetachSafe) with .when_done()
#[test]
fn test_openat_when_done_not_detach_safe() {
  lio::init();

  let path = CString::new("/tmp/lio_test_openat_when_done.txt").unwrap();

  let (sender, receiver) = mpsc::channel();
  let sender1 = sender.clone();

  let path_clone = path.clone();
  openat(
    libc::AT_FDCWD,
    path.clone(),
    libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
  )
  .when_done(move |result| {
    let fd = result.expect("OpenAt should succeed");
    unsafe {
      libc::close(fd);
      libc::unlink(path_clone.as_ptr());
    }
    sender1.send(()).unwrap();
  });

  // assert_eq!(receiver.try_recv().unwrap_err(), TryRecvError::Empty);

  lio::tick();

  receiver.recv().unwrap();
}

/// Test Shutdown (NOT DetachSafe) with .when_done()
#[test]
fn test_shutdown_when_done_not_detach_safe() {
  lio::init();

  let server_sock =
    unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
  let addr = "127.0.0.1:0".parse().unwrap();

  let (sender, receiver) = mpsc::channel();
  let sender1 = sender.clone();
  let sender2 = sender.clone();
  let sender3 = sender.clone();

  bind(server_sock, addr).when_done(move |result| {
    result.unwrap();
    sender1.send(()).unwrap();
  });

  // assert_eq!(receiver.try_recv().unwrap_err(), TryRecvError::Empty);

  lio::tick();

  receiver.recv().unwrap();

  listen(server_sock, 5).when_done(move |result| {
    result.unwrap();
    sender2.send(()).unwrap();
  });

  // assert_eq!(receiver.try_recv().unwrap_err(), TryRecvError::Empty);

  lio::tick();

  receiver.recv().unwrap();

  let mut sockaddr: libc::sockaddr_in = unsafe { std::mem::zeroed() };
  let mut addrlen = std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
  unsafe {
    libc::getsockname(
      server_sock,
      &mut sockaddr as *mut _ as *mut libc::sockaddr,
      &mut addrlen,
    );
  }
  let port = u16::from_be(sockaddr.sin_port);
  let connect_addr = format!("127.0.0.1:{}", port).parse().unwrap();

  let client_sock =
    unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };

  connect(client_sock, connect_addr).when_done(move |result| {
    result.unwrap();
    sender3.send(()).unwrap();
  });

  // assert_eq!(receiver.try_recv().unwrap_err(), TryRecvError::Empty);

  lio::tick();

  receiver.recv().unwrap();

  let (sender4, receiver4) = mpsc::channel();
  let sender5 = sender4.clone();

  shutdown(client_sock, libc::SHUT_WR).when_done(move |result| {
    assert!(result.is_ok());
    sender5.send(()).unwrap();
  });

  // assert_eq!(receiver4.try_recv().unwrap_err(), TryRecvError::Empty);

  lio::tick();

  receiver4.recv().unwrap();

  unsafe {
    libc::close(client_sock);
    libc::close(server_sock);
  }
}

/// Test Truncate (NOT DetachSafe) with .when_done()
#[test]
fn test_truncate_when_done_not_detach_safe() {
  lio::init();

  let path = CString::new("/tmp/lio_test_truncate_when_done.txt").unwrap();
  let fd = unsafe {
    libc::open(
      path.as_ptr(),
      libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
      0o644,
    )
  };

  let data = b"test data for truncate";
  unsafe {
    libc::write(fd, data.as_ptr() as *const libc::c_void, data.len());
  }

  let (sender, receiver) = mpsc::channel();
  let sender1 = sender.clone();

  truncate(fd, 10).when_done(move |result| {
    assert!(result.is_ok());
    sender1.send(()).unwrap();
  });

  // assert_eq!(receiver.try_recv().unwrap_err(), TryRecvError::Empty);

  lio::tick();

  receiver.recv().unwrap();

  unsafe {
    libc::close(fd);
    libc::unlink(path.as_ptr());
  }
}

// ============================================================================
// CONCURRENT TESTS
// ============================================================================

/// Test concurrent operations with mixed DetachSafe and non-DetachSafe
#[test]
fn test_concurrent_mixed_operations() {
  lio::init();

  let completed = Arc::new(Mutex::new(0));

  // DetachSafe operations
  let mut fds = [0i32; 2];
  unsafe {
    libc::pipe(fds.as_mut_ptr());
  }
  let c = completed.clone();
  close(fds[0]).when_done(move |_| {
    *c.lock().unwrap() += 1;
  });

  // Non-DetachSafe operation
  let path = CString::new("/tmp/lio_test_mixed_ops.txt").unwrap();
  let fd = unsafe {
    libc::open(
      path.as_ptr(),
      libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
      0o644,
    )
  };
  let c = completed.clone();
  write(fd, b"data".to_vec(), 0).when_done(move |(_, _)| {
    *c.lock().unwrap() += 1;
  });

  lio::tick();

  assert_eq!(*completed.lock().unwrap(), 2);

  unsafe {
    libc::close(fds[1]);
    libc::close(fd);
    libc::unlink(path.as_ptr());
  }
}
