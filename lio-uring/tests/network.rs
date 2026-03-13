//! Integration tests for network operations.

use lio_uring::operation::*;
use lio_uring::LioUring;
use std::io::{Read as IoRead, Write as IoWrite};
use std::net::{TcpListener, TcpStream, UdpSocket};
use std::os::fd::AsRawFd;
use std::thread;
use std::time::Duration;

fn get_free_port() -> u16 {
  let listener = TcpListener::bind("127.0.0.1:0").unwrap();
  listener.local_addr().unwrap().port()
}

// ============================================================================
// Socket Creation Tests
// ============================================================================

#[test]
fn test_socket_tcp_ipv4() {
  let mut ring = LioUring::new(8).unwrap();

  let op = Socket::new(libc::AF_INET, libc::SOCK_STREAM, 0);
  unsafe { ring.push(op.build(), 1) }.unwrap();
  ring.submit().unwrap();

  let completion = ring.wait().unwrap();
  assert!(completion.is_ok());
  let fd = completion.result();
  assert!(fd >= 0);

  unsafe { libc::close(fd) };
}

#[test]
fn test_socket_tcp_ipv6() {
  let mut ring = LioUring::new(8).unwrap();

  let op = Socket::new(libc::AF_INET6, libc::SOCK_STREAM, 0);
  unsafe { ring.push(op.build(), 1) }.unwrap();
  ring.submit().unwrap();

  let completion = ring.wait().unwrap();
  assert!(completion.is_ok());
  let fd = completion.result();
  assert!(fd >= 0);

  unsafe { libc::close(fd) };
}

#[test]
fn test_socket_udp() {
  let mut ring = LioUring::new(8).unwrap();

  let op = Socket::new(libc::AF_INET, libc::SOCK_DGRAM, 0);
  unsafe { ring.push(op.build(), 1) }.unwrap();
  ring.submit().unwrap();

  let completion = ring.wait().unwrap();
  assert!(completion.is_ok());
  let fd = completion.result();
  assert!(fd >= 0);

  unsafe { libc::close(fd) };
}

#[test]
fn test_socket_nonblocking() {
  let mut ring = LioUring::new(8).unwrap();

  let op =
    Socket::new(libc::AF_INET, libc::SOCK_STREAM | libc::SOCK_NONBLOCK, 0);
  unsafe { ring.push(op.build(), 1) }.unwrap();
  ring.submit().unwrap();

  let completion = ring.wait().unwrap();
  assert!(completion.is_ok());
  let fd = completion.result();
  assert!(fd >= 0);

  // Verify it's non-blocking
  let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
  assert!((flags & libc::O_NONBLOCK) != 0);

  unsafe { libc::close(fd) };
}

// ============================================================================
// Connect Tests
// ============================================================================

#[test]
fn test_connect_tcp() {
  let port = get_free_port();
  let listener = TcpListener::bind(format!("127.0.0.1:{}", port)).unwrap();

  let mut ring = LioUring::new(8).unwrap();

  // Create socket
  let op = Socket::new(libc::AF_INET, libc::SOCK_STREAM, 0);
  unsafe { ring.push(op.build(), 1) }.unwrap();
  ring.submit().unwrap();
  let sock_fd = ring.wait().unwrap().result();
  assert!(sock_fd >= 0);

  // Connect
  let addr = libc::sockaddr_in {
    sin_family: libc::AF_INET as u16,
    sin_port: port.to_be(),
    sin_addr: libc::in_addr {
      s_addr: u32::from_ne_bytes([127, 0, 0, 1]).to_be(),
    },
    sin_zero: [0; 8],
  };

  let op = Connect::new(
    sock_fd,
    &addr as *const _ as *const libc::sockaddr,
    std::mem::size_of::<libc::sockaddr_in>() as u32,
  );
  unsafe { ring.push(op.build(), 2) }.unwrap();
  ring.submit().unwrap();

  let completion = ring.wait().unwrap();
  assert!(completion.is_ok(), "connect failed: {}", completion.result());

  // Accept on server side
  let (_stream, _addr) = listener.accept().unwrap();

  unsafe { libc::close(sock_fd) };
}

#[test]
fn test_connect_refused() {
  let mut ring = LioUring::new(8).unwrap();

  // Create socket
  let op = Socket::new(libc::AF_INET, libc::SOCK_STREAM, 0);
  unsafe { ring.push(op.build(), 1) }.unwrap();
  ring.submit().unwrap();
  let sock_fd = ring.wait().unwrap().result();

  // Try to connect to a port that's not listening
  let addr = libc::sockaddr_in {
    sin_family: libc::AF_INET as u16,
    sin_port: 65432u16.to_be(), // Unlikely to be in use
    sin_addr: libc::in_addr {
      s_addr: u32::from_ne_bytes([127, 0, 0, 1]).to_be(),
    },
    sin_zero: [0; 8],
  };

  let op = Connect::new(
    sock_fd,
    &addr as *const _ as *const libc::sockaddr,
    std::mem::size_of::<libc::sockaddr_in>() as u32,
  );
  unsafe { ring.push(op.build(), 2) }.unwrap();
  ring.submit().unwrap();

  let completion = ring.wait().unwrap();
  assert!(!completion.is_ok());
  assert_eq!(completion.result(), -libc::ECONNREFUSED);

  unsafe { libc::close(sock_fd) };
}

// ============================================================================
// Accept Tests
// ============================================================================

#[test]
fn test_accept() {
  let port = get_free_port();
  let listener = TcpListener::bind(format!("127.0.0.1:{}", port)).unwrap();
  listener.set_nonblocking(true).unwrap();

  let mut ring = LioUring::new(8).unwrap();
  let listener_fd = listener.as_raw_fd();

  // Spawn client connection
  let client_handle = thread::spawn(move || {
    thread::sleep(Duration::from_millis(50));
    TcpStream::connect(format!("127.0.0.1:{}", port)).unwrap()
  });

  // Accept
  let mut client_addr: libc::sockaddr_in = unsafe { std::mem::zeroed() };
  let mut addr_len = std::mem::size_of::<libc::sockaddr_in>() as u32;

  let op = Accept::new(
    listener_fd,
    &mut client_addr as *mut _ as *mut libc::sockaddr,
    &mut addr_len,
  );
  unsafe { ring.push(op.build(), 1) }.unwrap();
  ring.submit().unwrap();

  let completion = ring.wait().unwrap();
  assert!(completion.is_ok(), "accept failed: {}", completion.result());
  let client_fd = completion.result();
  assert!(client_fd >= 0);

  client_handle.join().unwrap();
  unsafe { libc::close(client_fd) };
}

// ============================================================================
// Send/Recv Tests
// ============================================================================

#[test]
fn test_send_recv() {
  let port = get_free_port();
  let listener = TcpListener::bind(format!("127.0.0.1:{}", port)).unwrap();

  // Connect client
  let mut client = TcpStream::connect(format!("127.0.0.1:{}", port)).unwrap();
  let (mut server_stream, _) = listener.accept().unwrap();

  let mut ring = LioUring::new(8).unwrap();

  // Send from client (using io_uring)
  let send_data = b"hello from io_uring";
  let op =
    Send::new(client.as_raw_fd(), send_data.as_ptr(), send_data.len() as u32);
  unsafe { ring.push(op.build(), 1) }.unwrap();
  ring.submit().unwrap();

  let completion = ring.wait().unwrap();
  assert!(completion.is_ok());
  assert_eq!(completion.result(), send_data.len() as i32);

  // Receive on server (using io_uring)
  let mut recv_buf = [0u8; 64];
  let op = Recv::new(
    server_stream.as_raw_fd(),
    recv_buf.as_mut_ptr(),
    recv_buf.len() as u32,
  );
  unsafe { ring.push(op.build(), 2) }.unwrap();
  ring.submit().unwrap();

  let completion = ring.wait().unwrap();
  assert!(completion.is_ok());
  assert_eq!(completion.result(), send_data.len() as i32);
  assert_eq!(&recv_buf[..send_data.len()], send_data);
}

#[test]
fn test_send_recv_with_flags() {
  let port = get_free_port();
  let listener = TcpListener::bind(format!("127.0.0.1:{}", port)).unwrap();

  let client = TcpStream::connect(format!("127.0.0.1:{}", port)).unwrap();
  let (server_stream, _) = listener.accept().unwrap();

  let mut ring = LioUring::new(8).unwrap();

  // Send with MSG_DONTWAIT
  let send_data = b"test message";
  let op =
    Send::new(client.as_raw_fd(), send_data.as_ptr(), send_data.len() as u32)
      .flags(libc::MSG_DONTWAIT);
  unsafe { ring.push(op.build(), 1) }.unwrap();
  ring.submit().unwrap();

  let completion = ring.wait().unwrap();
  assert!(completion.is_ok());

  // Receive
  let mut recv_buf = [0u8; 64];
  let op = Recv::new(
    server_stream.as_raw_fd(),
    recv_buf.as_mut_ptr(),
    recv_buf.len() as u32,
  );
  unsafe { ring.push(op.build(), 2) }.unwrap();
  ring.submit().unwrap();

  let completion = ring.wait().unwrap();
  assert!(completion.is_ok());
}

#[test]
fn test_recv_closed_connection() {
  let port = get_free_port();
  let listener = TcpListener::bind(format!("127.0.0.1:{}", port)).unwrap();

  let client = TcpStream::connect(format!("127.0.0.1:{}", port)).unwrap();
  let (server_stream, _) = listener.accept().unwrap();

  // Close client
  drop(client);

  let mut ring = LioUring::new(8).unwrap();

  // Try to recv - should return 0 (EOF)
  let mut recv_buf = [0u8; 64];
  let op = Recv::new(
    server_stream.as_raw_fd(),
    recv_buf.as_mut_ptr(),
    recv_buf.len() as u32,
  );
  unsafe { ring.push(op.build(), 1) }.unwrap();
  ring.submit().unwrap();

  let completion = ring.wait().unwrap();
  assert!(completion.is_ok());
  assert_eq!(completion.result(), 0); // EOF
}

// ============================================================================
// Shutdown Tests
// ============================================================================

#[test]
fn test_shutdown_write() {
  let port = get_free_port();
  let listener = TcpListener::bind(format!("127.0.0.1:{}", port)).unwrap();

  let client = TcpStream::connect(format!("127.0.0.1:{}", port)).unwrap();
  let (_server_stream, _) = listener.accept().unwrap();

  let mut ring = LioUring::new(8).unwrap();

  let op = Shutdown::new(client.as_raw_fd(), libc::SHUT_WR);
  unsafe { ring.push(op.build(), 1) }.unwrap();
  ring.submit().unwrap();

  let completion = ring.wait().unwrap();
  assert!(completion.is_ok());
}

#[test]
fn test_shutdown_both() {
  let port = get_free_port();
  let listener = TcpListener::bind(format!("127.0.0.1:{}", port)).unwrap();

  let client = TcpStream::connect(format!("127.0.0.1:{}", port)).unwrap();
  let (_server_stream, _) = listener.accept().unwrap();

  let mut ring = LioUring::new(8).unwrap();

  let op = Shutdown::new(client.as_raw_fd(), libc::SHUT_RDWR);
  unsafe { ring.push(op.build(), 1) }.unwrap();
  ring.submit().unwrap();

  let completion = ring.wait().unwrap();
  assert!(completion.is_ok());
}

// ============================================================================
// Poll Tests
// ============================================================================

#[test]
fn test_poll_readable() {
  let port = get_free_port();
  let listener = TcpListener::bind(format!("127.0.0.1:{}", port)).unwrap();

  let mut client = TcpStream::connect(format!("127.0.0.1:{}", port)).unwrap();
  let (server_stream, _) = listener.accept().unwrap();

  // Write data to make server readable
  client.write_all(b"data").unwrap();

  let mut ring = LioUring::new(8).unwrap();

  let op = PollAdd::new(server_stream.as_raw_fd(), libc::POLLIN as u32);
  unsafe { ring.push(op.build(), 1) }.unwrap();
  ring.submit().unwrap();

  let completion = ring.wait().unwrap();
  assert!(completion.is_ok());
  assert!((completion.result() as u32 & libc::POLLIN as u32) != 0);
}

#[test]
fn test_poll_writable() {
  let port = get_free_port();
  let listener = TcpListener::bind(format!("127.0.0.1:{}", port)).unwrap();

  let client = TcpStream::connect(format!("127.0.0.1:{}", port)).unwrap();
  let (_server_stream, _) = listener.accept().unwrap();

  let mut ring = LioUring::new(8).unwrap();

  // TCP socket should be immediately writable after connect
  let op = PollAdd::new(client.as_raw_fd(), libc::POLLOUT as u32);
  unsafe { ring.push(op.build(), 1) }.unwrap();
  ring.submit().unwrap();

  let completion = ring.wait().unwrap();
  assert!(completion.is_ok());
  assert!((completion.result() as u32 & libc::POLLOUT as u32) != 0);
}

// ============================================================================
// Concurrent Network Operations
// ============================================================================

#[test]
fn test_concurrent_connections() {
  let port = get_free_port();
  let listener = TcpListener::bind(format!("127.0.0.1:{}", port)).unwrap();
  listener.set_nonblocking(true).unwrap();

  let mut ring = LioUring::new(32).unwrap();

  // Create and connect multiple sockets
  let mut socket_fds = Vec::new();
  for i in 0..4 {
    let op =
      Socket::new(libc::AF_INET, libc::SOCK_STREAM | libc::SOCK_NONBLOCK, 0);
    unsafe { ring.push(op.build(), i * 2) }.unwrap();
  }
  ring.submit().unwrap();

  for _ in 0..4 {
    let completion = ring.wait().unwrap();
    assert!(completion.is_ok());
    socket_fds.push(completion.result());
  }

  // Connect all sockets
  let addr = libc::sockaddr_in {
    sin_family: libc::AF_INET as u16,
    sin_port: port.to_be(),
    sin_addr: libc::in_addr {
      s_addr: u32::from_ne_bytes([127, 0, 0, 1]).to_be(),
    },
    sin_zero: [0; 8],
  };

  for (i, &fd) in socket_fds.iter().enumerate() {
    let op = Connect::new(
      fd,
      &addr as *const _ as *const libc::sockaddr,
      std::mem::size_of::<libc::sockaddr_in>() as u32,
    );
    unsafe { ring.push(op.build(), (i * 2 + 1) as u64) }.unwrap();
  }
  ring.submit().unwrap();

  // Accept connections on server side
  for _ in 0..4 {
    loop {
      match listener.accept() {
        Ok(_) => break,
        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
          thread::sleep(Duration::from_millis(10));
        }
        Err(e) => panic!("accept error: {}", e),
      }
    }
  }

  // Wait for connect completions
  for _ in 0..4 {
    let completion = ring.wait().unwrap();
    // EINPROGRESS is expected for non-blocking connect
    assert!(completion.is_ok() || completion.result() == -libc::EINPROGRESS);
  }

  for fd in socket_fds {
    unsafe { libc::close(fd) };
  }
}

#[test]
fn test_echo_server_pattern() {
  let port = get_free_port();
  let listener = TcpListener::bind(format!("127.0.0.1:{}", port)).unwrap();

  // Client thread
  let client_handle = thread::spawn(move || {
    thread::sleep(Duration::from_millis(50));
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", port)).unwrap();
    stream.write_all(b"ping").unwrap();

    let mut buf = [0u8; 4];
    stream.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, b"ping");
  });

  // Server using io_uring
  let (mut server_stream, _) = listener.accept().unwrap();
  let mut ring = LioUring::new(8).unwrap();

  // Recv
  let mut buf = [0u8; 64];
  let op =
    Recv::new(server_stream.as_raw_fd(), buf.as_mut_ptr(), buf.len() as u32);
  unsafe { ring.push(op.build(), 1) }.unwrap();
  ring.submit().unwrap();

  let completion = ring.wait().unwrap();
  assert!(completion.is_ok());
  let bytes_read = completion.result() as usize;

  // Echo back
  let op =
    Send::new(server_stream.as_raw_fd(), buf.as_ptr(), bytes_read as u32);
  unsafe { ring.push(op.build(), 2) }.unwrap();
  ring.submit().unwrap();

  let completion = ring.wait().unwrap();
  assert!(completion.is_ok());
  assert_eq!(completion.result(), bytes_read as i32);

  client_handle.join().unwrap();
}
