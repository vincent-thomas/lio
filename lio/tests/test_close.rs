//! Tests for close operation.

mod common;

use common::{TempFile, poll_until_recv};
use lio::api::resource::Resource;
use lio::{Lio, api};
use std::ffi::CString;
use std::os::fd::FromRawFd;
use std::sync::mpsc;

#[test]
fn test_close_basic() {
  let mut lio = Lio::new(64).unwrap();
  let temp = TempFile::new("close_basic");

  // Open a file
  let fd = unsafe {
    libc::open(
      temp.path.as_ptr(),
      libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC,
      0o644,
    )
  };
  assert!(fd >= 0, "Failed to open file");

  // Close it using lio
  let (sender, receiver) = mpsc::channel();
  api::close(fd).with_lio(&mut lio).send_with(sender);

  poll_until_recv(&mut lio, &receiver)
    .expect("Failed to close file descriptor");

  // Verify it's closed by trying to write to it (should fail)
  let result =
    unsafe { libc::write(fd, b"test".as_ptr() as *const libc::c_void, 4) };
  assert!(result < 0, "Writing to closed fd should fail");
}

#[test]
fn test_close_after_write() {
  let mut lio = Lio::new(64).unwrap();
  let temp = TempFile::new("close_after_write");

  // Open file
  let fd = unsafe {
    libc::open(
      temp.path.as_ptr(),
      libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC,
      0o644,
    )
  };
  assert!(fd >= 0, "Failed to open file");

  // Write data using libc
  let data = b"test data";
  unsafe {
    libc::write(fd, data.as_ptr() as *const libc::c_void, data.len());
  }

  // Close it using lio
  let (sender, receiver) = mpsc::channel();
  api::close(fd).with_lio(&mut lio).send_with(sender);

  poll_until_recv(&mut lio, &receiver)
    .expect("Failed to close file descriptor");

  // Verify data was written by reading it back
  unsafe {
    let read_fd = libc::open(temp.path.as_ptr(), libc::O_RDONLY);
    let mut buf = vec![0u8; 9];
    let read_bytes =
      libc::read(read_fd, buf.as_mut_ptr() as *mut libc::c_void, 9);
    assert_eq!(read_bytes, 9);
    assert_eq!(&buf, data);
    libc::close(read_fd);
  }
}

#[test]
fn test_close_socket() {
  let mut lio = Lio::new(64).unwrap();

  // Create a socket
  let sock_fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
  assert!(sock_fd >= 0, "Failed to create socket");

  // Close it
  let (sender, receiver) = mpsc::channel();
  api::close(sock_fd).with_lio(&mut lio).send_with(sender);

  poll_until_recv(&mut lio, &receiver).expect("Failed to close socket");

  // Verify it's closed by trying to use it (should fail)
  let result = unsafe {
    let mut addr: libc::sockaddr_in = std::mem::zeroed();
    addr.sin_family = libc::AF_INET as _;
    addr.sin_port = 0;
    addr.sin_addr = libc::in_addr { s_addr: 0 };
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
  let mut lio = Lio::new(64).unwrap();

  // Try to close an invalid file descriptor
  let (sender, receiver) = mpsc::channel();
  api::close(-1).with_lio(&mut lio).send_with(sender);

  let result = poll_until_recv(&mut lio, &receiver);
  assert!(result.is_err(), "Closing invalid fd should return error");
}

#[test]
#[cfg_attr(
  any(target_os = "freebsd", target_os = "macos"),
  ignore = "macOS/FreeBSD may silently succeed on double close"
)]
fn test_close_already_closed() {
  let mut lio = Lio::new(64).unwrap();
  let temp = TempFile::new("close_double");

  // Open a file
  let fd = unsafe {
    libc::open(
      temp.path.as_ptr(),
      libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC,
      0o644,
    )
  };
  assert!(fd >= 0, "Failed to open file");

  // Close it once
  let (sender, receiver) = mpsc::channel();
  api::close(fd).with_lio(&mut lio).send_with(sender);

  poll_until_recv(&mut lio, &receiver).expect("First close should succeed");

  // Try to close again - should fail
  let (sender2, receiver2) = mpsc::channel();
  api::close(fd).with_lio(&mut lio).send_with(sender2);

  let result = poll_until_recv(&mut lio, &receiver2);
  assert!(result.is_err(), "Closing already closed fd should fail");
}

#[test]
fn test_close_pipe() {
  let mut lio = Lio::new(64).unwrap();

  // Create a pipe
  let mut pipe_fds = [0i32; 2];
  unsafe {
    assert_eq!(libc::pipe(pipe_fds.as_mut_ptr()), 0);
  }

  let read_fd = pipe_fds[0];
  let write_fd = pipe_fds[1];

  // Close both ends
  let (sender, receiver) = mpsc::channel();
  api::close(write_fd).with_lio(&mut lio).send_with(sender.clone());

  poll_until_recv(&mut lio, &receiver).expect("Failed to close write end");

  api::close(read_fd).with_lio(&mut lio).send_with(sender);

  poll_until_recv(&mut lio, &receiver).expect("Failed to close read end");

  // Verify they're closed
  let result = unsafe {
    libc::write(write_fd, b"test".as_ptr() as *const libc::c_void, 4)
  };
  assert!(result < 0, "Writing to closed pipe should fail");
}

#[test]
fn test_close_concurrent() {
  let mut lio = Lio::new(64).unwrap();

  // Test closing multiple file descriptors
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

  // Submit all close operations
  for (fd, _) in &fds {
    api::close(*fd).with_lio(&mut lio).send_with(sender.clone());
  }

  // Wait for all closes
  for _ in 0..10 {
    poll_until_recv(&mut lio, &receiver).expect("Failed to close fd");
  }

  // Cleanup temp files
  for (_, path) in fds {
    unsafe {
      libc::unlink(path.as_ptr());
    }
  }
}

#[test]
fn test_close_after_read() {
  let mut lio = Lio::new(64).unwrap();
  let temp = TempFile::new("close_after_read");

  // Create file with data
  unsafe {
    let fd = libc::open(
      temp.path.as_ptr(),
      libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC,
      0o644,
    );
    libc::write(fd, b"test data".as_ptr() as *const libc::c_void, 9);
    libc::close(fd);
  }

  // Open for reading
  let fd = unsafe { libc::open(temp.path.as_ptr(), libc::O_RDONLY) };
  assert!(fd >= 0);

  // Read some data
  let mut buf = vec![0u8; 9];
  unsafe {
    libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, 9);
  }

  // Close it
  let (sender, receiver) = mpsc::channel();
  api::close(fd).with_lio(&mut lio).send_with(sender);

  poll_until_recv(&mut lio, &receiver)
    .expect("Failed to close file descriptor");
}

#[test]
fn test_close_with_resource_drop() {
  // Test that Resource properly closes on drop
  let temp = TempFile::new("close_resource_drop");

  // Open a file via libc
  let fd = unsafe {
    libc::open(
      temp.path.as_ptr(),
      libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC,
      0o644,
    )
  };
  assert!(fd >= 0, "Failed to open file");

  // Wrap in Resource and drop it
  {
    let resource = unsafe { Resource::from_raw_fd(fd) };
    assert!(resource.will_close(), "Resource should close on drop");
  }

  // fd should be closed now - verify by trying to use it
  let result =
    unsafe { libc::write(fd, b"test".as_ptr() as *const libc::c_void, 4) };
  assert!(result < 0, "Writing to closed fd should fail");
}

#[test]
fn test_close_resource_clone_behavior() {
  let temp = TempFile::new("close_resource_clone");

  let fd = unsafe {
    libc::open(
      temp.path.as_ptr(),
      libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC,
      0o644,
    )
  };
  assert!(fd >= 0, "Failed to open file");

  let resource = unsafe { Resource::from_raw_fd(fd) };
  assert!(resource.will_close());

  // Clone the resource
  let resource2 = resource.clone();

  // Neither should close while both exist
  assert!(!resource.will_close());
  assert!(!resource2.will_close());

  // Drop the clone
  drop(resource2);

  // Now original should close on drop
  assert!(resource.will_close());

  // Drop original - fd should be closed
  drop(resource);

  // Verify closed
  let result =
    unsafe { libc::write(fd, b"test".as_ptr() as *const libc::c_void, 4) };
  assert!(result < 0, "Writing to closed fd should fail");
}
