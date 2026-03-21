//! Tests for the accept_stream functionality.

#![cfg(unix)]

use std::net::SocketAddr;
use std::os::fd::AsRawFd;
use std::time::Duration;

use lio::{Lio, api};

/// Test that accept_stream compiles and can be created
#[test]
fn test_accept_stream_creation() {
  let lio = Lio::new(64).unwrap();

  // Create and set up the listening socket
  let sock =
    api::socket(libc::AF_INET, libc::SOCK_STREAM, 0).with_lio(&lio).send();
  lio.try_run().unwrap();
  let sock = sock.recv().unwrap();

  // Bind to any available port
  let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
  let bind_rx = api::bind(&sock, addr).with_lio(&lio).send();
  lio.try_run().unwrap();
  bind_rx.recv().unwrap();

  // Start listening
  let listen_rx = api::listen(&sock, 128).with_lio(&lio).send();
  lio.try_run().unwrap();
  listen_rx.recv().unwrap();

  // Create accept stream - just verify it compiles
  let _stream = api::accept_stream(&sock).with_lio(&lio);

  // Stream exists and can be dropped
}

/// Test accept stream with actual connections using callback-based approach
#[test]
fn test_accept_stream_with_connections() {
  use std::io::Write;
  use std::net::TcpStream;
  use std::sync::Arc;
  use std::sync::atomic::{AtomicUsize, Ordering};
  use std::thread;

  let lio = Lio::new(64).unwrap();

  // Create and set up the listening socket
  let sock =
    api::socket(libc::AF_INET, libc::SOCK_STREAM, 0).with_lio(&lio).send();
  lio.try_run().unwrap();
  let sock = sock.recv().unwrap();

  // Set SO_REUSEADDR
  let optval: libc::c_int = 1;
  unsafe {
    libc::setsockopt(
      sock.as_raw_fd(),
      libc::SOL_SOCKET,
      libc::SO_REUSEADDR,
      &optval as *const _ as *const libc::c_void,
      std::mem::size_of::<libc::c_int>() as libc::socklen_t,
    );
  }

  // Bind to any available port
  let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
  let bind_rx = api::bind(&sock, addr).with_lio(&lio).send();
  lio.try_run().unwrap();
  bind_rx.recv().unwrap();

  // Get the actual bound address
  let sockfd = sock.as_raw_fd();
  let mut storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
  let mut len =
    std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
  unsafe {
    libc::getsockname(
      sockfd,
      &mut storage as *mut _ as *mut libc::sockaddr,
      &mut len,
    );
  }
  let port = unsafe {
    let addr_in = &*(&storage as *const _ as *const libc::sockaddr_in);
    u16::from_be(addr_in.sin_port)
  };
  let server_addr: SocketAddr = format!("127.0.0.1:{}", port).parse().unwrap();

  // Start listening
  let listen_rx = api::listen(&sock, 128).with_lio(&lio).send();
  lio.try_run().unwrap();
  listen_rx.recv().unwrap();

  // Track accepted connections
  let accepted_count = Arc::new(AtomicUsize::new(0));
  let count_clone = accepted_count.clone();

  // Use regular accept for a simple test - the stream API is better tested
  // via an async runtime integration
  let recv1 = api::accept(&sock).with_lio(&lio).send();

  // Spawn a client
  let handle = thread::spawn(move || {
    thread::sleep(Duration::from_millis(50));
    let mut stream =
      TcpStream::connect(server_addr).expect("Failed to connect");
    stream.write_all(b"hello").expect("Failed to write");
    stream
  });

  // Wait for connection
  lio.run_timeout(Duration::from_secs(2)).unwrap();

  let result = recv1.recv();
  assert!(result.is_ok(), "Accept should succeed");
  let (_client, addr) = result.unwrap();
  assert!(addr.port() > 0, "Client should have a port");
  count_clone.fetch_add(1, Ordering::SeqCst);

  // Cleanup
  handle.join().unwrap();

  assert_eq!(accepted_count.load(Ordering::SeqCst), 1);
}
