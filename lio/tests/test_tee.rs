#![cfg(linux)]

use lio::Lio;
use lio::api::{self, resource::Resource};
use std::os::fd::FromRawFd;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

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
#[ignore]
fn test_tee_basic() {
  let mut lio = Lio::new(64).unwrap();

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

  // Use tee to copy data from pipe1 to pipe2
  api::tee(
    &unsafe { Resource::from_raw_fd(pipe1_read) },
    &unsafe { Resource::from_raw_fd(pipe2_write) },
    test_data.len() as u32,
  )
  .with_lio(&mut lio)
  .send_with(sender.clone());

  let bytes_copied =
    poll_until_recv(&mut lio, &receiver).expect("Failed to tee data");
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
  unsafe {
    libc::close(pipe1_fds[0]);
    libc::close(pipe1_fds[1]);
    libc::close(pipe2_fds[0]);
    libc::close(pipe2_fds[1]);
  }
}

#[test]
#[ignore]
fn test_tee_large_data() {
  let mut lio = Lio::new(64).unwrap();

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

  // Tee the data
  api::tee(
    &unsafe { Resource::from_raw_fd(pipe1_read) },
    &unsafe { Resource::from_raw_fd(pipe2_write) },
    test_data.len() as u32,
  )
  .with_lio(&mut lio)
  .send_with(sender);

  let bytes_copied =
    poll_until_recv(&mut lio, &receiver).expect("Failed to tee large data");

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
  unsafe {
    libc::close(pipe1_fds[0]);
    libc::close(pipe1_fds[1]);
    libc::close(pipe2_fds[0]);
    libc::close(pipe2_fds[1]);
  }
}

#[test]
#[ignore]
fn test_tee_partial() {
  let mut lio = Lio::new(64).unwrap();

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

  // Tee only part of the data
  let bytes_to_tee = 8;
  api::tee(
    &unsafe { Resource::from_raw_fd(pipe1_read) },
    &unsafe { Resource::from_raw_fd(pipe2_write) },
    bytes_to_tee,
  )
  .with_lio(&mut lio)
  .send_with(sender);

  let bytes_copied =
    poll_until_recv(&mut lio, &receiver).expect("Failed to tee partial data");
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
  unsafe {
    libc::close(pipe1_fds[1]);
    libc::close(pipe2_fds[0]);
    libc::close(pipe2_fds[1]);
  }
}

#[test]
#[ignore]
fn test_tee_empty_pipe() {
  let mut lio = Lio::new(64).unwrap();

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

  // Try to tee from empty pipe
  api::tee(
    &unsafe { Resource::from_raw_fd(pipe1_read) },
    &unsafe { Resource::from_raw_fd(pipe2_write) },
    100,
  )
  .with_lio(&mut lio)
  .send_with(sender);

  let result = poll_until_recv(&mut lio, &receiver);

  // Should fail with EAGAIN or similar
  assert!(result.is_err(), "Tee from empty pipe should fail");

  // Cleanup
  unsafe {
    libc::close(pipe1_fds[0]);
    libc::close(pipe1_fds[1]);
    libc::close(pipe2_fds[0]);
    libc::close(pipe2_fds[1]);
  }
}

#[test]
#[ignore]
fn test_tee_zero_size() {
  let mut lio = Lio::new(64).unwrap();

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

  // Tee with size 0
  api::tee(
    &unsafe { Resource::from_raw_fd(pipe1_read) },
    &unsafe { Resource::from_raw_fd(pipe2_write) },
    0,
  )
  .with_lio(&mut lio)
  .send_with(sender);

  let bytes_copied = poll_until_recv(&mut lio, &receiver)
    .expect("Tee with size 0 should succeed");
  assert_eq!(bytes_copied, 0);

  // Cleanup
  unsafe {
    libc::close(pipe1_fds[0]);
    libc::close(pipe1_fds[1]);
    libc::close(pipe2_fds[0]);
    libc::close(pipe2_fds[1]);
  }
}

#[cfg(target_os = "linux")]
#[ignore]
#[test]
fn test_tee_multiple() {
  let mut lio = Lio::new(64).unwrap();

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

  // Tee to pipe2
  api::tee(
    &unsafe { Resource::from_raw_fd(pipe1_read) },
    &unsafe { Resource::from_raw_fd(pipe2_write) },
    test_data.len() as u32,
  )
  .with_lio(&mut lio)
  .send_with(sender.clone());

  let bytes1 = poll_until_recv(&mut lio, &receiver).expect("First tee failed");
  assert_eq!(bytes1 as usize, test_data.len());

  let (sender2, receiver2) = mpsc::channel();

  // Tee to pipe3 (data still in pipe1)
  api::tee(
    &unsafe { Resource::from_raw_fd(pipe1_read) },
    &unsafe { Resource::from_raw_fd(pipe3_write) },
    test_data.len() as u32,
  )
  .with_lio(&mut lio)
  .send_with(sender2);

  let bytes2 =
    poll_until_recv(&mut lio, &receiver2).expect("Second tee failed");
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
  unsafe {
    libc::close(pipe1_fds[0]);
    libc::close(pipe1_fds[1]);
    libc::close(pipe2_fds[0]);
    libc::close(pipe2_fds[1]);
    libc::close(pipe3_fds[0]);
    libc::close(pipe3_fds[1]);
  }
}

#[test]
#[ignore]
fn test_tee_concurrent() {
  let mut lio = Lio::new(64).unwrap();

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
    let data_len = data.len();
    api::tee(
      &unsafe { Resource::from_raw_fd(pipe1_fds[0]) },
      &unsafe { Resource::from_raw_fd(pipe2_fds[1]) },
      data_len as u32,
    )
    .with_lio(&mut lio)
    .send_with(sender.clone());
  }

  for (_, _, data) in &tasks {
    let bytes_copied =
      poll_until_recv(&mut lio, &receiver).expect("Concurrent tee failed");
    assert_eq!(bytes_copied as usize, data.len());
  }

  // Cleanup
  for (pipe1_fds, pipe2_fds, _) in &tasks {
    unsafe {
      libc::close(pipe1_fds[0]);
      libc::close(pipe1_fds[1]);
      libc::close(pipe2_fds[0]);
      libc::close(pipe2_fds[1]);
    }
  }
}
