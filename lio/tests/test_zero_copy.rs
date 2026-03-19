//! Tests for zero-copy operations: splice, sendfile, copy_file_range.

mod common;

use common::{TempFile, poll_until_recv};
use lio::api::resource::Resource;
use lio::{Lio, api};
use std::os::fd::FromRawFd;
use std::sync::mpsc;

// ============================================================================
// SendFile tests (Linux only - macOS requires socket as output)
// ============================================================================

#[cfg(target_os = "linux")]
#[test]
fn test_sendfile_to_file() {
  let mut lio = Lio::new(64).unwrap();
  let src_file = TempFile::new("sendfile_src");
  let dst_file = TempFile::new("sendfile_dst");

  // Create source file with data
  let test_data = b"Hello, sendfile world! This is test data.";
  unsafe {
    let fd = libc::open(
      src_file.path.as_ptr(),
      libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC,
      0o644,
    );
    assert!(fd >= 0, "Failed to create source file");
    libc::write(fd, test_data.as_ptr() as *const _, test_data.len());
    libc::close(fd);
  }

  // Create destination file
  unsafe {
    let fd = libc::open(
      dst_file.path.as_ptr(),
      libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC,
      0o644,
    );
    libc::close(fd);
  }

  // Open both files
  let cwd = unsafe { Resource::from_raw_fd(libc::AT_FDCWD) };

  let (sender_src, receiver_src) = mpsc::channel();
  api::openat(&cwd, src_file.path.clone(), libc::O_RDONLY)
    .with_lio(&mut lio)
    .send_with(sender_src);
  let src_fd =
    poll_until_recv(&mut lio, &receiver_src).expect("Failed to open source");

  let (sender_dst, receiver_dst) = mpsc::channel();
  api::openat(&cwd, dst_file.path.clone(), libc::O_WRONLY)
    .with_lio(&mut lio)
    .send_with(sender_dst);
  let dst_fd =
    poll_until_recv(&mut lio, &receiver_dst).expect("Failed to open dest");

  // Sendfile
  let (sender, receiver) = mpsc::channel();
  api::sendfile(&dst_fd, &src_fd, Some(0), test_data.len())
    .with_lio(&mut lio)
    .send_with(sender);

  let bytes = poll_until_recv(&mut lio, &receiver).expect("sendfile failed");
  assert_eq!(bytes as usize, test_data.len(), "Should copy all bytes");

  // Verify destination content
  unsafe {
    let fd = libc::open(dst_file.path.as_ptr(), libc::O_RDONLY);
    let mut buf = vec![0u8; 100];
    let n = libc::read(fd, buf.as_mut_ptr() as *mut _, 100);
    libc::close(fd);
    assert_eq!(n as usize, test_data.len());
    assert_eq!(&buf[..test_data.len()], test_data);
  }

  std::mem::forget(cwd);
}

#[cfg(target_os = "linux")]
#[test]
fn test_sendfile_partial() {
  let mut lio = Lio::new(64).unwrap();
  let src_file = TempFile::new("sendfile_partial_src");
  let dst_file = TempFile::new("sendfile_partial_dst");

  // Create source file with data
  let test_data = b"0123456789ABCDEFGHIJ";
  unsafe {
    let fd = libc::open(
      src_file.path.as_ptr(),
      libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC,
      0o644,
    );
    libc::write(fd, test_data.as_ptr() as *const _, test_data.len());
    libc::close(fd);
  }

  // Create destination file
  unsafe {
    let fd = libc::open(
      dst_file.path.as_ptr(),
      libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC,
      0o644,
    );
    libc::close(fd);
  }

  let cwd = unsafe { Resource::from_raw_fd(libc::AT_FDCWD) };

  let (sender_src, receiver_src) = mpsc::channel();
  api::openat(&cwd, src_file.path.clone(), libc::O_RDONLY)
    .with_lio(&mut lio)
    .send_with(sender_src);
  let src_fd =
    poll_until_recv(&mut lio, &receiver_src).expect("Failed to open source");

  let (sender_dst, receiver_dst) = mpsc::channel();
  api::openat(&cwd, dst_file.path.clone(), libc::O_WRONLY)
    .with_lio(&mut lio)
    .send_with(sender_dst);
  let dst_fd =
    poll_until_recv(&mut lio, &receiver_dst).expect("Failed to open dest");

  // Sendfile with offset (start at position 10, copy 5 bytes)
  let (sender, receiver) = mpsc::channel();
  api::sendfile(&dst_fd, &src_fd, Some(10), 5)
    .with_lio(&mut lio)
    .send_with(sender);

  let bytes = poll_until_recv(&mut lio, &receiver).expect("sendfile failed");
  assert_eq!(bytes, 5, "Should copy 5 bytes");

  // Verify destination content
  unsafe {
    let fd = libc::open(dst_file.path.as_ptr(), libc::O_RDONLY);
    let mut buf = vec![0u8; 10];
    let n = libc::read(fd, buf.as_mut_ptr() as *mut _, 10);
    libc::close(fd);
    assert_eq!(n, 5);
    assert_eq!(&buf[..5], b"ABCDE");
  }

  std::mem::forget(cwd);
}

/// Test sendfile to a socket (works on all Unix platforms)
#[cfg(unix)]
#[test]
fn test_sendfile_to_socket() {
  use std::net::SocketAddr;

  let mut lio = Lio::new(64).unwrap();
  let src_file = TempFile::new("sendfile_socket_src");

  // Create source file with data
  let test_data = b"Sendfile to socket test!";
  unsafe {
    let fd = libc::open(
      src_file.path.as_ptr(),
      libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC,
      0o644,
    );
    assert!(fd >= 0, "Failed to create source file");
    libc::write(fd, test_data.as_ptr() as *const _, test_data.len());
    libc::close(fd);
  }

  // Create a TCP socket pair for testing
  let (sender_sock, receiver_sock) = mpsc::channel();
  api::socket(libc::AF_INET, libc::SOCK_STREAM, 0)
    .with_lio(&mut lio)
    .send_with(sender_sock.clone());
  let server_sock = poll_until_recv(&mut lio, &receiver_sock)
    .expect("Failed to create server socket");

  // Bind to any port
  let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
  let (sender_unit, receiver_unit) = mpsc::channel();
  api::bind(&server_sock, addr)
    .with_lio(&mut lio)
    .send_with(sender_unit.clone());
  poll_until_recv(&mut lio, &receiver_unit).expect("Failed to bind");

  // Get bound address
  let bound_addr = unsafe {
    let mut addr_storage = std::mem::MaybeUninit::<libc::sockaddr_in>::zeroed();
    let mut addr_len =
      std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
    libc::getsockname(
      std::os::fd::AsRawFd::as_raw_fd(&server_sock),
      addr_storage.as_mut_ptr() as *mut libc::sockaddr,
      &mut addr_len,
    );
    let sockaddr_in = addr_storage.assume_init();
    let port = u16::from_be(sockaddr_in.sin_port);
    format!("127.0.0.1:{}", port).parse::<SocketAddr>().unwrap()
  };

  // Listen
  api::listen(&server_sock, 1)
    .with_lio(&mut lio)
    .send_with(sender_unit.clone());
  poll_until_recv(&mut lio, &receiver_unit).expect("Failed to listen");

  // Create client socket
  api::socket(libc::AF_INET, libc::SOCK_STREAM, 0)
    .with_lio(&mut lio)
    .send_with(sender_sock.clone());
  let client_sock = poll_until_recv(&mut lio, &receiver_sock)
    .expect("Failed to create client socket");

  // Connect and accept
  let (sender_connect, receiver_connect) = mpsc::channel();
  let (sender_accept, receiver_accept) = mpsc::channel();
  api::connect(&client_sock, bound_addr)
    .with_lio(&mut lio)
    .send_with(sender_connect);
  api::accept(&server_sock).with_lio(&mut lio).send_with(sender_accept);

  poll_until_recv(&mut lio, &receiver_connect).expect("Failed to connect");
  let (accepted_sock, _) =
    poll_until_recv(&mut lio, &receiver_accept).expect("Failed to accept");

  // Open source file
  let cwd = unsafe { Resource::from_raw_fd(libc::AT_FDCWD) };
  let (sender_open, receiver_open) = mpsc::channel();
  api::openat(&cwd, src_file.path.clone(), libc::O_RDONLY)
    .with_lio(&mut lio)
    .send_with(sender_open);
  let src_fd =
    poll_until_recv(&mut lio, &receiver_open).expect("Failed to open source");

  // Sendfile from file to socket
  let (sender, receiver) = mpsc::channel();
  api::sendfile(&accepted_sock, &src_fd, Some(0), test_data.len())
    .with_lio(&mut lio)
    .send_with(sender);

  let bytes = poll_until_recv(&mut lio, &receiver).expect("sendfile failed");
  assert_eq!(bytes as usize, test_data.len(), "Should send all bytes");

  // Receive on client socket
  let buf = vec![0u8; 50];
  let (sender_recv, receiver_recv) = mpsc::channel();
  api::recv(&client_sock, buf, None).with_lio(&mut lio).send_with(sender_recv);
  let (result, buf) = poll_until_recv(&mut lio, &receiver_recv);
  let n = result.expect("recv failed") as usize;

  assert_eq!(n, test_data.len());
  assert_eq!(&buf[..n], test_data);

  std::mem::forget(cwd);
}

// ============================================================================
// Splice tests (Linux only)
// ============================================================================

#[cfg(target_os = "linux")]
#[test]
fn test_splice_pipe_to_file() {
  let mut lio = Lio::new(64).unwrap();
  let dst_file = TempFile::new("splice_dst");

  // Create a pipe
  let mut pipe_fds = [0i32; 2];
  unsafe {
    let ret = libc::pipe(pipe_fds.as_mut_ptr());
    assert_eq!(ret, 0, "Failed to create pipe");
  }
  let pipe_read = unsafe { Resource::from_raw_fd(pipe_fds[0]) };
  let pipe_write_fd = pipe_fds[1];

  // Write data to pipe
  let test_data = b"Splice test data!";
  unsafe {
    libc::write(pipe_write_fd, test_data.as_ptr() as *const _, test_data.len());
  }

  // Create destination file
  let cwd = unsafe { Resource::from_raw_fd(libc::AT_FDCWD) };
  let (sender_open, receiver_open) = mpsc::channel();
  api::openat(
    &cwd,
    dst_file.path.clone(),
    libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC,
  )
  .with_lio(&mut lio)
  .send_with(sender_open);
  let dst_fd = poll_until_recv(&mut lio, &receiver_open)
    .expect("Failed to create dest file");

  // Splice from pipe to file
  let (sender, receiver) = mpsc::channel();
  api::splice(&pipe_read, None, &dst_fd, Some(0), test_data.len() as u32, 0)
    .with_lio(&mut lio)
    .send_with(sender);

  let bytes = poll_until_recv(&mut lio, &receiver).expect("splice failed");
  assert_eq!(bytes as usize, test_data.len(), "Should splice all bytes");

  // Verify destination content
  drop(dst_fd);
  unsafe {
    let fd = libc::open(dst_file.path.as_ptr(), libc::O_RDONLY);
    let mut buf = vec![0u8; 50];
    let n = libc::read(fd, buf.as_mut_ptr() as *mut _, 50);
    libc::close(fd);
    assert_eq!(n as usize, test_data.len());
    assert_eq!(&buf[..test_data.len()], test_data);
  }

  // Cleanup
  unsafe {
    libc::close(pipe_write_fd);
  }
  std::mem::forget(cwd);
}

#[cfg(target_os = "linux")]
#[test]
fn test_splice_file_to_pipe() {
  let mut lio = Lio::new(64).unwrap();
  let src_file = TempFile::new("splice_src");

  // Create source file with data
  let test_data = b"File to pipe splice!";
  unsafe {
    let fd = libc::open(
      src_file.path.as_ptr(),
      libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC,
      0o644,
    );
    libc::write(fd, test_data.as_ptr() as *const _, test_data.len());
    libc::close(fd);
  }

  // Create a pipe
  let mut pipe_fds = [0i32; 2];
  unsafe {
    let ret = libc::pipe(pipe_fds.as_mut_ptr());
    assert_eq!(ret, 0, "Failed to create pipe");
  }
  let pipe_read_fd = pipe_fds[0];
  let pipe_write = unsafe { Resource::from_raw_fd(pipe_fds[1]) };

  // Open source file
  let cwd = unsafe { Resource::from_raw_fd(libc::AT_FDCWD) };
  let (sender_open, receiver_open) = mpsc::channel();
  api::openat(&cwd, src_file.path.clone(), libc::O_RDONLY)
    .with_lio(&mut lio)
    .send_with(sender_open);
  let src_fd = poll_until_recv(&mut lio, &receiver_open)
    .expect("Failed to open source file");

  // Splice from file to pipe
  let (sender, receiver) = mpsc::channel();
  api::splice(&src_fd, Some(0), &pipe_write, None, test_data.len() as u32, 0)
    .with_lio(&mut lio)
    .send_with(sender);

  let bytes = poll_until_recv(&mut lio, &receiver).expect("splice failed");
  assert_eq!(bytes as usize, test_data.len(), "Should splice all bytes");

  // Read from pipe to verify
  unsafe {
    let mut buf = vec![0u8; 50];
    let n = libc::read(pipe_read_fd, buf.as_mut_ptr() as *mut _, 50);
    assert_eq!(n as usize, test_data.len());
    assert_eq!(&buf[..test_data.len()], test_data);
    libc::close(pipe_read_fd);
  }

  std::mem::forget(cwd);
}

// ============================================================================
// CopyFileRange tests (Linux only)
// ============================================================================

#[cfg(target_os = "linux")]
#[test]
fn test_copy_file_range_basic() {
  let mut lio = Lio::new(64).unwrap();
  let src_file = TempFile::new("copy_range_src");
  let dst_file = TempFile::new("copy_range_dst");

  // Create source file with data
  let test_data = b"Copy file range test data!";
  unsafe {
    let fd = libc::open(
      src_file.path.as_ptr(),
      libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC,
      0o644,
    );
    libc::write(fd, test_data.as_ptr() as *const _, test_data.len());
    libc::close(fd);
  }

  // Create destination file
  unsafe {
    let fd = libc::open(
      dst_file.path.as_ptr(),
      libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC,
      0o644,
    );
    libc::close(fd);
  }

  let cwd = unsafe { Resource::from_raw_fd(libc::AT_FDCWD) };

  // Open source file
  let (sender_src, receiver_src) = mpsc::channel();
  api::openat(&cwd, src_file.path.clone(), libc::O_RDONLY)
    .with_lio(&mut lio)
    .send_with(sender_src);
  let src_fd =
    poll_until_recv(&mut lio, &receiver_src).expect("Failed to open source");

  // Open dest file for writing
  let (sender_dst, receiver_dst) = mpsc::channel();
  api::openat(&cwd, dst_file.path.clone(), libc::O_WRONLY)
    .with_lio(&mut lio)
    .send_with(sender_dst);
  let dst_fd =
    poll_until_recv(&mut lio, &receiver_dst).expect("Failed to open dest");

  // Copy file range
  let (sender, receiver) = mpsc::channel();
  api::copy_file_range(&src_fd, 0, &dst_fd, 0, test_data.len(), 0)
    .with_lio(&mut lio)
    .send_with(sender);

  let bytes =
    poll_until_recv(&mut lio, &receiver).expect("copy_file_range failed");
  assert_eq!(bytes as usize, test_data.len(), "Should copy all bytes");

  // Verify destination content
  drop(dst_fd);
  unsafe {
    let fd = libc::open(dst_file.path.as_ptr(), libc::O_RDONLY);
    let mut buf = vec![0u8; 50];
    let n = libc::read(fd, buf.as_mut_ptr() as *mut _, 50);
    libc::close(fd);
    assert_eq!(n as usize, test_data.len());
    assert_eq!(&buf[..test_data.len()], test_data);
  }

  std::mem::forget(cwd);
}

#[cfg(target_os = "linux")]
#[test]
fn test_copy_file_range_with_offset() {
  let mut lio = Lio::new(64).unwrap();
  let src_file = TempFile::new("copy_range_offset_src");
  let dst_file = TempFile::new("copy_range_offset_dst");

  // Create source file with data
  let test_data = b"0123456789ABCDEFGHIJ";
  unsafe {
    let fd = libc::open(
      src_file.path.as_ptr(),
      libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC,
      0o644,
    );
    libc::write(fd, test_data.as_ptr() as *const _, test_data.len());
    libc::close(fd);
  }

  // Create destination file with some initial content
  unsafe {
    let fd = libc::open(
      dst_file.path.as_ptr(),
      libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC,
      0o644,
    );
    libc::write(fd, b"____________________".as_ptr() as *const _, 20);
    libc::close(fd);
  }

  let cwd = unsafe { Resource::from_raw_fd(libc::AT_FDCWD) };

  let (sender_src, receiver_src) = mpsc::channel();
  api::openat(&cwd, src_file.path.clone(), libc::O_RDONLY)
    .with_lio(&mut lio)
    .send_with(sender_src);
  let src_fd =
    poll_until_recv(&mut lio, &receiver_src).expect("Failed to open source");

  let (sender_dst, receiver_dst) = mpsc::channel();
  api::openat(&cwd, dst_file.path.clone(), libc::O_WRONLY)
    .with_lio(&mut lio)
    .send_with(sender_dst);
  let dst_fd =
    poll_until_recv(&mut lio, &receiver_dst).expect("Failed to open dest");

  // Copy bytes 10-15 from source to bytes 5-10 in destination
  let (sender, receiver) = mpsc::channel();
  api::copy_file_range(&src_fd, 10, &dst_fd, 5, 5, 0)
    .with_lio(&mut lio)
    .send_with(sender);

  let bytes =
    poll_until_recv(&mut lio, &receiver).expect("copy_file_range failed");
  assert_eq!(bytes, 5, "Should copy 5 bytes");

  // Verify destination content
  drop(dst_fd);
  unsafe {
    let fd = libc::open(dst_file.path.as_ptr(), libc::O_RDONLY);
    let mut buf = vec![0u8; 25];
    let n = libc::read(fd, buf.as_mut_ptr() as *mut _, 25);
    libc::close(fd);
    assert_eq!(n, 20);
    // Should be "_____ABCDE__________"
    assert_eq!(&buf[5..10], b"ABCDE");
  }

  std::mem::forget(cwd);
}

#[cfg(target_os = "linux")]
#[test]
fn test_copy_file_range_large() {
  let mut lio = Lio::new(64).unwrap();
  let src_file = TempFile::new("copy_range_large_src");
  let dst_file = TempFile::new("copy_range_large_dst");

  // Create source file with larger data (1MB)
  let test_data: Vec<u8> = (0..1024 * 1024).map(|i| (i % 256) as u8).collect();
  unsafe {
    let fd = libc::open(
      src_file.path.as_ptr(),
      libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC,
      0o644,
    );
    libc::write(fd, test_data.as_ptr() as *const _, test_data.len());
    libc::close(fd);
  }

  // Create empty destination file
  unsafe {
    let fd = libc::open(
      dst_file.path.as_ptr(),
      libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC,
      0o644,
    );
    libc::close(fd);
  }

  let cwd = unsafe { Resource::from_raw_fd(libc::AT_FDCWD) };

  let (sender_src, receiver_src) = mpsc::channel();
  api::openat(&cwd, src_file.path.clone(), libc::O_RDONLY)
    .with_lio(&mut lio)
    .send_with(sender_src);
  let src_fd =
    poll_until_recv(&mut lio, &receiver_src).expect("Failed to open source");

  let (sender_dst, receiver_dst) = mpsc::channel();
  api::openat(&cwd, dst_file.path.clone(), libc::O_WRONLY)
    .with_lio(&mut lio)
    .send_with(sender_dst);
  let dst_fd =
    poll_until_recv(&mut lio, &receiver_dst).expect("Failed to open dest");

  // Copy entire file
  let (sender, receiver) = mpsc::channel();
  api::copy_file_range(&src_fd, 0, &dst_fd, 0, test_data.len(), 0)
    .with_lio(&mut lio)
    .send_with(sender);

  let bytes =
    poll_until_recv(&mut lio, &receiver).expect("copy_file_range failed");
  assert_eq!(bytes as usize, test_data.len(), "Should copy all bytes");

  // Verify destination size
  drop(dst_fd);
  unsafe {
    let mut stat: libc::stat = std::mem::zeroed();
    libc::stat(dst_file.path.as_ptr(), &mut stat);
    assert_eq!(stat.st_size as usize, test_data.len());
  }

  std::mem::forget(cwd);
}
