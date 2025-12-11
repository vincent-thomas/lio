#![cfg(linux)]

use lio::tee;
use std::sync::mpsc::{self, TryRecvError};

#[test]
#[ignore]
fn test_tee_basic() {
  lio::init();

  // Create two pipes
  let mut pipe1_fds = [0i32; 2];
  let mut pipe2_fds = [0i32; 2];

  unsafe {
    assert_eq!(libc::pipe(pipe1_fds.as_mut_ptr()), 0);
    assert_eq!(libc::pipe(pipe2_fds.as_mut_ptr()), 0);
  }

  let pipe1_read = pipe1_fds[0];
  let pipe1_write = pipe1_fds[1];
  let pipe2_write = pipe2_fds[1];
  let pipe2_read = pipe2_fds[0];

  // Write data to pipe1
  let test_data = b"Hello, tee!";
  unsafe {
    let written = libc::write(
      pipe1_write,
      test_data.as_ptr() as *const libc::c_void,
      test_data.len(),
    );
    assert_eq!(written, test_data.len() as isize);
  }

  let (sender, receiver) = mpsc::channel();
  let sender1 = sender.clone();

  // Use tee to copy data from pipe1 to pipe2
  tee(pipe1_read, pipe2_write, test_data.len() as u32).when_done(move |t| {
    sender1.send(t).unwrap();
  });

  assert_eq!(receiver.try_recv().unwrap_err(), TryRecvError::Empty);

  lio::tick();

  let bytes_copied = receiver.recv().unwrap().expect("Failed to tee data");
  assert_eq!(bytes_copied as usize, test_data.len());

  // Read from pipe2 to verify
  let mut buf2 = vec![0u8; test_data.len()];
  unsafe {
    let read_bytes = libc::read(
      pipe2_read,
      buf2.as_mut_ptr() as *mut libc::c_void,
      buf2.len(),
    );
    assert_eq!(read_bytes, test_data.len() as isize);
  }
  assert_eq!(&buf2, test_data);

  // Data should still be in pipe1
  let mut buf1 = vec![0u8; test_data.len()];
  unsafe {
    let read_bytes = libc::read(
      pipe1_read,
      buf1.as_mut_ptr() as *mut libc::c_void,
      buf1.len(),
    );
    assert_eq!(read_bytes, test_data.len() as isize);
  }
  assert_eq!(&buf1, test_data);

  // Cleanup
  let (sender2, receiver2) = mpsc::channel();
  let sender3 = sender2.clone();
  let sender4 = sender2.clone();
  let sender5 = sender2.clone();
  let sender6 = sender2.clone();

  lio::close(pipe1_fds[0]).when_done(move |t| {
    sender3.send(t).unwrap();
  });
  lio::close(pipe1_fds[1]).when_done(move |t| {
    sender4.send(t).unwrap();
  });
  lio::close(pipe2_fds[0]).when_done(move |t| {
    sender5.send(t).unwrap();
  });
  lio::close(pipe2_fds[1]).when_done(move |t| {
    sender6.send(t).unwrap();
  });

  assert_eq!(receiver2.try_recv().unwrap_err(), TryRecvError::Empty);

  lio::tick();

  receiver2.recv().unwrap().unwrap();
  receiver2.recv().unwrap().unwrap();
  receiver2.recv().unwrap().unwrap();
  receiver2.recv().unwrap().unwrap();
}

#[test]
#[ignore]
fn test_tee_large_data() {
  lio::init();

  let mut pipe1_fds = [0i32; 2];
  let mut pipe2_fds = [0i32; 2];

  unsafe {
    assert_eq!(libc::pipe(pipe1_fds.as_mut_ptr()), 0);
    assert_eq!(libc::pipe(pipe2_fds.as_mut_ptr()), 0);
  }

  let pipe1_read = pipe1_fds[0];
  let pipe1_write = pipe1_fds[1];
  let pipe2_write = pipe2_fds[1];
  let pipe2_read = pipe2_fds[0];

  // Write larger data
  let test_data: Vec<u8> = (0..4096).map(|i| (i % 256) as u8).collect();
  unsafe {
    let written = libc::write(
      pipe1_write,
      test_data.as_ptr() as *const libc::c_void,
      test_data.len(),
    );
    assert_eq!(written, test_data.len() as isize);
  }

  let (sender, receiver) = mpsc::channel();
  let sender1 = sender.clone();

  // Tee the data
  tee(pipe1_read, pipe2_write, test_data.len() as u32).when_done(move |t| {
    sender1.send(t).unwrap();
  });

  assert_eq!(receiver.try_recv().unwrap_err(), TryRecvError::Empty);

  lio::tick();

  let bytes_copied =
    receiver.recv().unwrap().expect("Failed to tee large data");

  assert!(bytes_copied > 0);
  assert!(bytes_copied as usize <= test_data.len());

  // Read from pipe2
  let mut buf2 = vec![0u8; bytes_copied as usize];
  unsafe {
    let read_bytes = libc::read(
      pipe2_read,
      buf2.as_mut_ptr() as *mut libc::c_void,
      buf2.len(),
    );
    assert_eq!(read_bytes, bytes_copied as isize);
  }
  assert_eq!(&buf2, &test_data[..bytes_copied as usize]);

  // Cleanup
  let (sender2, receiver2) = mpsc::channel();
  let sender3 = sender2.clone();
  let sender4 = sender2.clone();
  let sender5 = sender2.clone();
  let sender6 = sender2.clone();

  lio::close(pipe1_fds[0]).when_done(move |t| {
    sender3.send(t).unwrap();
  });
  lio::close(pipe1_fds[1]).when_done(move |t| {
    sender4.send(t).unwrap();
  });
  lio::close(pipe2_fds[0]).when_done(move |t| {
    sender5.send(t).unwrap();
  });
  lio::close(pipe2_fds[1]).when_done(move |t| {
    sender6.send(t).unwrap();
  });

  assert_eq!(receiver2.try_recv().unwrap_err(), TryRecvError::Empty);

  lio::tick();

  receiver2.recv().unwrap().unwrap();
  receiver2.recv().unwrap().unwrap();
  receiver2.recv().unwrap().unwrap();
  receiver2.recv().unwrap().unwrap();
}

#[test]
#[ignore]
fn test_tee_partial() {
  lio::init();

  let mut pipe1_fds = [0i32; 2];
  let mut pipe2_fds = [0i32; 2];

  unsafe {
    assert_eq!(libc::pipe(pipe1_fds.as_mut_ptr()), 0);
    assert_eq!(libc::pipe(pipe2_fds.as_mut_ptr()), 0);
  }

  let pipe1_read = pipe1_fds[0];
  let pipe1_write = pipe1_fds[1];
  let pipe2_write = pipe2_fds[1];
  let pipe2_read = pipe2_fds[0];

  let test_data = b"0123456789ABCDEF";
  unsafe {
    libc::write(
      pipe1_write,
      test_data.as_ptr() as *const libc::c_void,
      test_data.len(),
    );
  }

  let (sender, receiver) = mpsc::channel();
  let sender1 = sender.clone();

  // Tee only part of the data
  let bytes_to_tee = 8;
  tee(pipe1_read, pipe2_write, bytes_to_tee).when_done(move |t| {
    sender1.send(t).unwrap();
  });

  assert_eq!(receiver.try_recv().unwrap_err(), TryRecvError::Empty);

  lio::tick();

  let bytes_copied =
    receiver.recv().unwrap().expect("Failed to tee partial data");
  assert_eq!(bytes_copied, bytes_to_tee as i32);

  // Read from pipe2
  let mut buf2 = vec![0u8; bytes_to_tee as usize];
  unsafe {
    let read_bytes = libc::read(
      pipe2_read,
      buf2.as_mut_ptr() as *mut libc::c_void,
      buf2.len(),
    );
    assert_eq!(read_bytes, bytes_to_tee as isize);
  }
  assert_eq!(&buf2, &test_data[..bytes_to_tee as usize]);

  // All data should still be in pipe1
  let mut buf1 = vec![0u8; test_data.len()];
  unsafe {
    let read_bytes = libc::read(
      pipe1_read,
      buf1.as_mut_ptr() as *mut libc::c_void,
      buf1.len(),
    );
    assert_eq!(read_bytes, test_data.len() as isize);
  }
  assert_eq!(&buf1, test_data);

  // Cleanup
  let (sender2, receiver2) = mpsc::channel();
  let sender3 = sender2.clone();
  let sender4 = sender2.clone();
  let sender5 = sender2.clone();

  lio::close(pipe1_fds[1]).when_done(move |t| {
    sender3.send(t).unwrap();
  });
  lio::close(pipe2_fds[0]).when_done(move |t| {
    sender4.send(t).unwrap();
  });
  lio::close(pipe2_fds[1]).when_done(move |t| {
    sender5.send(t).unwrap();
  });

  assert_eq!(receiver2.try_recv().unwrap_err(), TryRecvError::Empty);

  lio::tick();

  receiver2.recv().unwrap().unwrap();
  receiver2.recv().unwrap().unwrap();
  receiver2.recv().unwrap().unwrap();
}

#[test]
#[ignore]
fn test_tee_empty_pipe() {
  lio::init();

  let mut pipe1_fds = [0i32; 2];
  let mut pipe2_fds = [0i32; 2];

  unsafe {
    assert_eq!(libc::pipe(pipe1_fds.as_mut_ptr()), 0);
    assert_eq!(libc::pipe(pipe2_fds.as_mut_ptr()), 0);
  }

  let pipe1_read = pipe1_fds[0];
  let pipe2_write = pipe2_fds[1];

  // Set pipes to non-blocking
  unsafe {
    let flags = libc::fcntl(pipe1_read, libc::F_GETFL, 0);
    libc::fcntl(pipe1_read, libc::F_SETFL, flags | libc::O_NONBLOCK);
  }

  let (sender, receiver) = mpsc::channel();
  let sender1 = sender.clone();

  // Try to tee from empty pipe
  tee(pipe1_read, pipe2_write, 100).when_done(move |t| {
    sender1.send(t).unwrap();
  });

  assert_eq!(receiver.try_recv().unwrap_err(), TryRecvError::Empty);

  lio::tick();

  let result = receiver.recv().unwrap();

  // Should fail with EAGAIN or similar
  assert!(result.is_err(), "Tee from empty pipe should fail");

  // Cleanup
  let (sender2, receiver2) = mpsc::channel();
  let sender3 = sender2.clone();
  let sender4 = sender2.clone();
  let sender5 = sender2.clone();
  let sender6 = sender2.clone();

  lio::close(pipe1_fds[0]).when_done(move |t| {
    sender3.send(t).unwrap();
  });
  lio::close(pipe1_fds[1]).when_done(move |t| {
    sender4.send(t).unwrap();
  });
  lio::close(pipe2_fds[0]).when_done(move |t| {
    sender5.send(t).unwrap();
  });
  lio::close(pipe2_fds[1]).when_done(move |t| {
    sender6.send(t).unwrap();
  });

  assert_eq!(receiver2.try_recv().unwrap_err(), TryRecvError::Empty);

  lio::tick();

  receiver2.recv().unwrap().unwrap();
  receiver2.recv().unwrap().unwrap();
  receiver2.recv().unwrap().unwrap();
  receiver2.recv().unwrap().unwrap();
}

#[test]
#[ignore]
fn test_tee_zero_size() {
  lio::init();

  let mut pipe1_fds = [0i32; 2];
  let mut pipe2_fds = [0i32; 2];

  unsafe {
    assert_eq!(libc::pipe(pipe1_fds.as_mut_ptr()), 0);
    assert_eq!(libc::pipe(pipe2_fds.as_mut_ptr()), 0);
  }

  let pipe1_read = pipe1_fds[0];
  let pipe1_write = pipe1_fds[1];
  let pipe2_write = pipe2_fds[1];

  let test_data = b"Some data";
  unsafe {
    libc::write(
      pipe1_write,
      test_data.as_ptr() as *const libc::c_void,
      test_data.len(),
    );
  }

  let (sender, receiver) = mpsc::channel();
  let sender1 = sender.clone();

  // Tee with size 0
  tee(pipe1_read, pipe2_write, 0).when_done(move |t| {
    sender1.send(t).unwrap();
  });

  assert_eq!(receiver.try_recv().unwrap_err(), TryRecvError::Empty);

  lio::tick();

  let bytes_copied =
    receiver.recv().unwrap().expect("Tee with size 0 should succeed");
  assert_eq!(bytes_copied, 0);

  // Cleanup
  let (sender2, receiver2) = mpsc::channel();
  let sender3 = sender2.clone();
  let sender4 = sender2.clone();
  let sender5 = sender2.clone();
  let sender6 = sender2.clone();

  lio::close(pipe1_fds[0]).when_done(move |t| {
    sender3.send(t).unwrap();
  });
  lio::close(pipe1_fds[1]).when_done(move |t| {
    sender4.send(t).unwrap();
  });
  lio::close(pipe2_fds[0]).when_done(move |t| {
    sender5.send(t).unwrap();
  });
  lio::close(pipe2_fds[1]).when_done(move |t| {
    sender6.send(t).unwrap();
  });

  assert_eq!(receiver2.try_recv().unwrap_err(), TryRecvError::Empty);

  lio::tick();

  receiver2.recv().unwrap().unwrap();
  receiver2.recv().unwrap().unwrap();
  receiver2.recv().unwrap().unwrap();
  receiver2.recv().unwrap().unwrap();
}

#[cfg(target_os = "linux")]
#[ignore]
#[test]
fn test_tee_multiple() {
  lio::init();

  let mut pipe1_fds = [0i32; 2];
  let mut pipe2_fds = [0i32; 2];
  let mut pipe3_fds = [0i32; 2];

  unsafe {
    assert_eq!(libc::pipe(pipe1_fds.as_mut_ptr()), 0);
    assert_eq!(libc::pipe(pipe2_fds.as_mut_ptr()), 0);
    assert_eq!(libc::pipe(pipe3_fds.as_mut_ptr()), 0);
  }

  let pipe1_read = pipe1_fds[0];
  let pipe1_write = pipe1_fds[1];
  let pipe2_write = pipe2_fds[1];
  let pipe2_read = pipe2_fds[0];
  let pipe3_write = pipe3_fds[1];
  let pipe3_read = pipe3_fds[0];

  let test_data = b"Tee multiple times";
  unsafe {
    libc::write(
      pipe1_write,
      test_data.as_ptr() as *const libc::c_void,
      test_data.len(),
    );
  }

  let (sender, receiver) = mpsc::channel();
  let sender1 = sender.clone();

  // Tee to pipe2
  tee(pipe1_read, pipe2_write, test_data.len() as u32).when_done(move |t| {
    sender1.send(t).unwrap();
  });

  assert_eq!(receiver.try_recv().unwrap_err(), TryRecvError::Empty);

  lio::tick();

  let bytes1 = receiver.recv().unwrap().expect("First tee failed");
  assert_eq!(bytes1 as usize, test_data.len());

  let (sender2, receiver2) = mpsc::channel();
  let sender3 = sender2.clone();

  // Tee to pipe3 (data still in pipe1)
  tee(pipe1_read, pipe3_write, test_data.len() as u32).when_done(move |t| {
    sender3.send(t).unwrap();
  });

  assert_eq!(receiver2.try_recv().unwrap_err(), TryRecvError::Empty);

  lio::tick();

  let bytes2 = receiver2.recv().unwrap().expect("Second tee failed");
  assert_eq!(bytes2 as usize, test_data.len());

  // Verify data in pipe2
  let mut buf2 = vec![0u8; test_data.len()];
  unsafe {
    libc::read(pipe2_read, buf2.as_mut_ptr() as *mut libc::c_void, buf2.len());
  }
  assert_eq!(&buf2, test_data);

  // Verify data in pipe3
  let mut buf3 = vec![0u8; test_data.len()];
  unsafe {
    libc::read(pipe3_read, buf3.as_mut_ptr() as *mut libc::c_void, buf3.len());
  }
  assert_eq!(&buf3, test_data);

  // Cleanup
  let (sender4, receiver4) = mpsc::channel();
  let sender5 = sender4.clone();
  let sender6 = sender4.clone();
  let sender7 = sender4.clone();
  let sender8 = sender4.clone();
  let sender9 = sender4.clone();
  let sender10 = sender4.clone();

  lio::close(pipe1_fds[0]).when_done(move |t| {
    sender5.send(t).unwrap();
  });
  lio::close(pipe1_fds[1]).when_done(move |t| {
    sender6.send(t).unwrap();
  });
  lio::close(pipe2_fds[0]).when_done(move |t| {
    sender7.send(t).unwrap();
  });
  lio::close(pipe2_fds[1]).when_done(move |t| {
    sender8.send(t).unwrap();
  });
  lio::close(pipe3_fds[0]).when_done(move |t| {
    sender9.send(t).unwrap();
  });
  lio::close(pipe3_fds[1]).when_done(move |t| {
    sender10.send(t).unwrap();
  });

  assert_eq!(receiver4.try_recv().unwrap_err(), TryRecvError::Empty);

  lio::tick();

  receiver4.recv().unwrap().unwrap();
  receiver4.recv().unwrap().unwrap();
  receiver4.recv().unwrap().unwrap();
  receiver4.recv().unwrap().unwrap();
  receiver4.recv().unwrap().unwrap();
  receiver4.recv().unwrap().unwrap();
}

#[test]
#[ignore]
fn test_tee_concurrent() {
  lio::init();

  // Test multiple concurrent tee operations
  let tasks: Vec<_> = (0..5)
    .map(|i| {
      let mut pipe1_fds = [0i32; 2];
      let mut pipe2_fds = [0i32; 2];

      unsafe {
        libc::pipe(pipe1_fds.as_mut_ptr());
        libc::pipe(pipe2_fds.as_mut_ptr());
      }

      let data = format!("Task {}", i);
      unsafe {
        libc::write(
          pipe1_fds[1],
          data.as_ptr() as *const libc::c_void,
          data.len(),
        );
      }

      (pipe1_fds, pipe2_fds, data)
    })
    .collect();

  let (sender, receiver) = mpsc::channel();

  for (pipe1_fds, pipe2_fds, data) in &tasks {
    let sender_clone = sender.clone();
    let data_len = data.len();
    tee(pipe1_fds[0], pipe2_fds[1], data_len as u32).when_done(move |t| {
      sender_clone.send(t).unwrap();
    });
  }

  assert_eq!(receiver.try_recv().unwrap_err(), TryRecvError::Empty);

  lio::tick();

  for (_, _, data) in &tasks {
    let bytes_copied = receiver.recv().unwrap().expect("Concurrent tee failed");
    assert_eq!(bytes_copied as usize, data.len());
  }

  // Cleanup
  let (sender2, receiver2) = mpsc::channel();

  for (pipe1_fds, pipe2_fds, _) in &tasks {
    let sender3 = sender2.clone();
    let sender4 = sender2.clone();
    let sender5 = sender2.clone();
    let sender6 = sender2.clone();

    lio::close(pipe1_fds[0]).when_done(move |t| {
      sender3.send(t).unwrap();
    });
    lio::close(pipe1_fds[1]).when_done(move |t| {
      sender4.send(t).unwrap();
    });
    lio::close(pipe2_fds[0]).when_done(move |t| {
      sender5.send(t).unwrap();
    });
    lio::close(pipe2_fds[1]).when_done(move |t| {
      sender6.send(t).unwrap();
    });
  }

  assert_eq!(receiver2.try_recv().unwrap_err(), TryRecvError::Empty);

  lio::tick();

  for _ in 0..20 {
    receiver2.recv().unwrap().unwrap();
  }
}
