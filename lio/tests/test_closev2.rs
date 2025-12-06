use lio::close;
use std::{
  ffi::CString,
  sync::mpsc::{self, TryRecvError},
};

#[test]
fn test_close_basic() {
  lio::init();

  let path = CString::new("/tmp/lio_test_close_basic.txt").unwrap();

  // Open a file
  let fd = unsafe {
    libc::open(
      path.as_ptr(),
      libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC,
      0o644,
    )
  };
  assert!(fd >= 0, "Failed to open file");

  let (sender, receiver) = mpsc::channel();

  let sender1 = sender.clone();

  // Close it
  close(fd).when_done(move |t| {
    sender1.send(t).unwrap();
  });

  assert_eq!(receiver.try_recv().unwrap_err(), TryRecvError::Empty);

  lio::tick();

  assert_eq!(receiver.try_recv().unwrap().unwrap(), ());

  // Verify it's closed
  let result =
    unsafe { libc::write(fd, b"test".as_ptr() as *const libc::c_void, 4) };
  assert!(result < 0, "Writing to closed fd should fail");

  // Cleanup
  unsafe {
    libc::unlink(path.as_ptr());
  }
}

#[test]
fn test_close_after_read() {
  lio::init();
  let path = CString::new("/tmp/lio_test_close_after_read.txt").unwrap();

  // Create file with data
  unsafe {
    let fd = libc::open(
      path.as_ptr(),
      libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC,
      0o644,
    );
    libc::write(fd, b"test data".as_ptr() as *const libc::c_void, 9);
    libc::close(fd);
  }

  // Open for reading
  let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDONLY) };
  assert!(fd >= 0);

  // Read some data
  let mut buf = vec![0u8; 9];
  unsafe {
    libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, 9);
  }

  let (sender, receiver) = mpsc::channel();

  let sender1 = sender.clone();

  // Close it
  close(fd).when_done(move |t| {
    sender1.send(t).unwrap();
  });

  assert_eq!(receiver.try_recv().unwrap_err(), TryRecvError::Empty);

  lio::tick();
  assert!(
    receiver.recv().unwrap().is_ok_and(|ret| ret == ()),
    "error with close syscall"
  );

  // Cleanup
  unsafe {
    libc::unlink(path.as_ptr());
  }
}

#[test]
fn test_close_after_write() {
  lio::init();
  let path = CString::new("/tmp/lio_test_close_after_write.txt").unwrap();

  // Open file
  let fd = unsafe {
    libc::open(
      path.as_ptr(),
      libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC,
      0o644,
    )
  };

  // Write data
  unsafe {
    libc::write(fd, b"test data".as_ptr() as *const libc::c_void, 9);
  }

  let (sender, receiver) = mpsc::channel();

  let sender1 = sender.clone();

  // Close it
  close(fd).when_done(move |t| {
    sender1.send(t).unwrap();
  });

  assert_eq!(receiver.try_recv().unwrap_err(), TryRecvError::Empty);

  lio::tick();

  assert!(
    receiver.recv().unwrap().is_ok_and(|ret| ret == ()),
    "not valid close fd"
  );

  // Verify data was written
  unsafe {
    let read_fd = libc::open(path.as_ptr(), libc::O_RDONLY);
    let mut buf = vec![0u8; 9];
    let read_bytes =
      libc::read(read_fd, buf.as_mut_ptr() as *mut libc::c_void, 9);
    assert_eq!(read_bytes, 9);
    assert_eq!(&buf, b"test data");
    libc::close(read_fd);
    libc::unlink(path.as_ptr());
  }
}

#[test]
fn test_close_socket() {
  lio::init();
  // Create a socket
  let sock_fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
  assert!(sock_fd >= 0, "Failed to create socket");

  let (sender, receiver) = mpsc::channel();

  let sender1 = sender.clone();

  // Close it
  close(sock_fd).when_done(move |t| {
    sender1.send(t).unwrap();
  });

  assert_eq!(receiver.try_recv().unwrap_err(), TryRecvError::Empty);

  lio::tick();

  assert!(
    receiver.recv().unwrap().is_ok_and(|ret| ret == ()),
    "Failed to close socket"
  );

  // Verify it's closed by trying to use it (should fail)
  let result = unsafe {
    let addr = libc::sockaddr_in {
      sin_family: libc::AF_INET as _,
      sin_port: 0,
      sin_addr: libc::in_addr { s_addr: 0 },
      sin_zero: [0; 8],
      #[cfg(not(linux))]
      sin_len: 0,
    };
    libc::bind(
      sock_fd,
      &addr as *const _ as *const libc::sockaddr,
      std::mem::size_of::<libc::sockaddr_in>() as u32,
    )
  };
  assert!(result < 0, "Bind on closed socket should fail");
}

#[test]
fn test_close_invalid_fd() {
  lio::init();

  let (sender, receiver) = mpsc::channel();

  let sender1 = sender.clone();

  // Close it
  close(-1).when_done(move |t| {
    sender1.send(t).unwrap();
  });

  assert_eq!(receiver.try_recv().unwrap_err(), TryRecvError::Empty);

  lio::tick();

  let err = receiver.recv().unwrap();

  assert!(err.is_err(), "Closing invalid fd should return error");
}

#[test]
fn test_close_already_closed() {
  lio::init();
  let path = CString::new("/tmp/lio_test_close_double.txt").unwrap();

  // Open a file
  let fd = unsafe {
    libc::open(
      path.as_ptr(),
      libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC,
      0o644,
    )
  };

  let (sender, receiver) = mpsc::channel();
  let sender1 = sender.clone();

  // Close it once
  close(fd).when_done(move |t| {
    sender1.send(t).unwrap();
  });

  assert_eq!(receiver.try_recv().unwrap_err(), TryRecvError::Empty);

  lio::tick();

  assert!(receiver.recv().unwrap().is_ok(), "First close should succeed");

  let (sender2, receiver2) = mpsc::channel();
  let sender3 = sender2.clone();

  // Try to close again - should fail
  close(fd).when_done(move |t| {
    sender3.send(t).unwrap();
  });

  assert_eq!(receiver2.try_recv().unwrap_err(), TryRecvError::Empty);

  lio::tick();

  assert!(
    receiver2.recv().unwrap().is_err(),
    "Closing already closed fd should fail"
  );

  // Cleanup
  unsafe {
    libc::unlink(path.as_ptr());
  }
}

#[test]
fn test_close_concurrent() {
  lio::init();

  // Test closing multiple file descriptors sequentially
  let fds: Vec<_> = (0..10)
    .map(|i| {
      let path =
        CString::new(format!("/tmp/lio_test_close_concurrent_{}.txt", i))
          .unwrap();
      let fd = unsafe {
        libc::open(
          path.as_ptr(),
          libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC,
          0o644,
        )
      };
      (fd, path)
    })
    .collect();

  let (sender, receiver) = mpsc::channel();

  for (fd, _) in &fds {
    let sender_clone = sender.clone();
    close(*fd).when_done(move |t| {
      sender_clone.send(t).unwrap();
    });
  }

  assert_eq!(receiver.try_recv().unwrap_err(), TryRecvError::Empty);

  lio::tick();

  for _ in 0..10 {
    assert!(receiver.recv().unwrap().is_ok(), "Failed to close fd");
  }

  for (_, path) in fds {
    unsafe {
      libc::unlink(path.as_ptr());
    }
  }
}

#[test]
fn test_close_pipe() {
  lio::init();

  // Create a pipe
  let mut pipe_fds = [0i32; 2];
  unsafe {
    assert_eq!(libc::pipe(pipe_fds.as_mut_ptr()), 0);
  }

  let read_fd = pipe_fds[0];
  let write_fd = pipe_fds[1];

  let (sender, receiver) = mpsc::channel();

  let sender1 = sender.clone();
  let sender2 = sender.clone();

  // Close both ends
  close(write_fd).when_done(move |t| {
    sender1.send(t).unwrap();
  });

  close(read_fd).when_done(move |t| {
    sender2.send(t).unwrap();
  });

  assert_eq!(receiver.try_recv().unwrap_err(), TryRecvError::Empty);

  lio::tick();

  assert!(receiver.recv().unwrap().is_ok(), "Failed to close write end");
  assert!(receiver.recv().unwrap().is_ok(), "Failed to close read end");

  // Verify they're closed
  let result = unsafe {
    libc::write(write_fd, b"test".as_ptr() as *const libc::c_void, 4)
  };
  assert!(result < 0, "Writing to closed pipe should fail");
}
