mod common;

use lio::*;
use std::ffi::CString;
use std::sync::mpsc;

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
