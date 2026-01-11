use lio::api;
use std::mem::MaybeUninit;
use std::net::SocketAddr;
use std::os::fd::AsRawFd;

#[test]
fn test_shutdown_write() {
  lio::init();

  // Create server socket
  let server_sock = lio::test_utils::tcp_socket().send();

  lio::tick();

  let server_sock = server_sock.recv().unwrap();

  let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
  let mut bind_recv = api::bind(&server_sock, addr).send();

  lio::tick();

  bind_recv.recv().expect("Failed to bind");

  let bound_addr = unsafe {
    let mut addr_storage = MaybeUninit::<libc::sockaddr_in>::zeroed();
    let mut addr_len =
      std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
    libc::getsockname(
      server_sock.as_raw_fd(),
      addr_storage.as_mut_ptr() as *mut libc::sockaddr,
      &mut addr_len,
    );
    let sockaddr_in = addr_storage.assume_init();
    let port = u16::from_be(sockaddr_in.sin_port);
    format!("127.0.0.1:{}", port).parse::<SocketAddr>().unwrap()
  };

  let listen_recv = api::listen(&server_sock, 128).send();

  lio::tick();

  listen_recv.recv().expect("Failed to listen");

  // Create client socket
  let client_sock = lio::test_utils::tcp_socket().send();

  lio::tick();

  let client_sock = client_sock.recv().unwrap();

  // Connect client to server
  let connect_recv = api::connect(&client_sock, bound_addr).send();

  lio::tick();

  connect_recv.recv().unwrap();

  // Accept connection on server
  let mut accept_recv = api::accept(&server_sock).send();

  lio::tick();
  lio::tick();

  let (server_client_fd, _addr) = accept_recv.recv().unwrap();

  // Shutdown write on client
  let mut shutdown_recv = api::shutdown(&client_sock, libc::SHUT_WR).send();

  lio::tick();

  shutdown_recv.recv().expect("Failed to shutdown write");

  // Try to send data (should fail or return 0)
  let data = b"Test data".to_vec();
  let mut send_recv = api::send(&client_sock, data, None).send();

  lio::tick();

  let (result, _) = send_recv.recv();

  // Send after SHUT_WR should fail
  assert!(
    result.is_err() || result.unwrap() == 0,
    "Send should fail after SHUT_WR"
  );

  // Server should be able to read EOF
  let buf = vec![0u8; 100];
  let mut recv_recv = api::recv(&server_client_fd, buf, None).send();

  lio::tick();

  let (bytes_received, _) = recv_recv.recv();
  assert_eq!(
    bytes_received.expect("Recv should succeed"),
    0,
    "Should receive EOF after client shutdown write"
  );

  // Cleanup
  unsafe {
    libc::close(client_sock.as_raw_fd());
    libc::close(server_client_fd.as_raw_fd());
    libc::close(server_sock.as_raw_fd());
  }
}

#[test]
fn test_shutdown_read() {
  lio::init();

  let mut server_sock = lio::test_utils::tcp_socket().send();

  lio::tick();

  let server_sock = server_sock.recv().unwrap();

  let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
  let mut bind_recv = api::bind(&server_sock, addr).send();

  lio::tick();

  bind_recv.recv().expect("Failed to bind");

  let bound_addr = unsafe {
    let mut addr_storage = MaybeUninit::<libc::sockaddr_in>::zeroed();
    let mut addr_len =
      std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
    libc::getsockname(
      server_sock.as_raw_fd(),
      addr_storage.as_mut_ptr() as *mut libc::sockaddr,
      &mut addr_len,
    );
    let sockaddr_in = addr_storage.assume_init();
    let port = u16::from_be(sockaddr_in.sin_port);
    format!("127.0.0.1:{}", port).parse::<SocketAddr>().unwrap()
  };

  let mut listen_recv = api::listen(&server_sock, 128).send();

  lio::tick();

  listen_recv.recv().expect("Failed to listen");

  // Create client socket
  let mut client_sock = lio::test_utils::tcp_socket().send();

  lio::tick();

  let client_sock = client_sock.recv().unwrap();

  // Connect client to server
  let mut connect_recv = api::connect(&client_sock, bound_addr).send();

  lio::tick();

  connect_recv.recv().unwrap();

  // Accept connection on server
  let mut accept_recv = api::accept(&server_sock).send();

  lio::tick();
  lio::tick();

  let (server_client_fd, _addr) = accept_recv.recv().unwrap();

  // Shutdown read on client
  let mut shutdown_recv = api::shutdown(&client_sock, libc::SHUT_RD).send();

  lio::tick();

  shutdown_recv.recv().expect("Failed to shutdown read");

  // Client can still send
  let data = b"Hello".to_vec();
  let mut send_recv = api::send(&client_sock, data.clone(), None).send();

  lio::tick();

  let (bytes_sent, _) = send_recv.recv();
  assert_eq!(bytes_sent.expect("Send should succeed") as usize, data.len());

  // Server can receive
  let buf = vec![0u8; 100];
  let mut recv_recv = api::recv(&server_client_fd, buf, None).send();

  lio::tick();

  let (bytes_received, received_buf) = recv_recv.recv();
  let bytes_received = bytes_received.expect("Recv should succeed") as usize;
  assert_eq!(bytes_received, data.len());
  assert_eq!(&received_buf[..bytes_received], data.as_slice());

  // Cleanup
  unsafe {
    libc::close(client_sock.as_raw_fd());
    libc::close(server_client_fd.as_raw_fd());
    libc::close(server_sock.as_raw_fd());
  }
}

#[test]
#[ignore = "flaky shutdown test"]
fn test_shutdown_both() {
  lio::init();

  let mut server_sock = lio::test_utils::tcp_socket().send();

  lio::tick();

  let server_sock = server_sock.recv().unwrap();

  let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
  let mut bind_recv = api::bind(&server_sock, addr).send();

  lio::tick();

  bind_recv.recv().expect("Failed to bind");

  let bound_addr = unsafe {
    let mut addr_storage = MaybeUninit::<libc::sockaddr_in>::zeroed();
    let mut addr_len =
      std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
    libc::getsockname(
      server_sock.as_raw_fd(),
      addr_storage.as_mut_ptr() as *mut libc::sockaddr,
      &mut addr_len,
    );
    let sockaddr_in = addr_storage.assume_init();
    let port = u16::from_be(sockaddr_in.sin_port);
    format!("127.0.0.1:{}", port).parse::<SocketAddr>().unwrap()
  };

  let mut listen_recv = api::listen(&server_sock, 128).send();

  lio::tick();

  listen_recv.recv().expect("Failed to listen");

  // Create client socket
  let mut client_sock = lio::test_utils::tcp_socket().send();

  lio::tick();

  let client_sock = client_sock.recv().unwrap();

  // Connect client to server
  let mut connect_recv = api::connect(&client_sock, bound_addr).send();

  lio::tick();

  connect_recv.recv().unwrap();

  // Accept connection on server
  let mut accept_recv = api::accept(&server_sock).send();

  lio::tick();
  lio::tick();

  let (server_client_fd, _addr) = accept_recv.recv().unwrap();

  // Shutdown both directions on client
  let mut shutdown_recv = api::shutdown(&client_sock, libc::SHUT_RDWR).send();

  lio::tick();

  shutdown_recv.recv().expect("Failed to shutdown both");

  // Send should fail
  let data = b"Test".to_vec();
  let mut send_recv = api::send(&client_sock, data, None).send();

  lio::tick();

  let (result, _) = send_recv.recv();
  assert!(
    result.is_err() || result.unwrap() == 0,
    "Send should fail after SHUT_RDWR"
  );

  // Server should receive EOF
  let buf = vec![0u8; 100];
  let mut recv_recv = api::recv(&server_client_fd, buf, None).send();

  lio::tick();

  let (bytes_received, _) = recv_recv.recv();
  assert_eq!(
    bytes_received.expect("Recv should succeed"),
    0,
    "Should receive EOF"
  );

  // Cleanup
  unsafe {
    libc::close(client_sock.as_raw_fd());
    libc::close(server_client_fd.as_raw_fd());
    libc::close(server_sock.as_raw_fd());
  }
}

// #[test]
// fn test_shutdown_invalid_fd() {
//   lio::init();
//
//   // Try to shutdown an invalid file descriptor
//   let mut shutdown_recv = api::shutdown(-1, libc::SHUT_RDWR).send();
//
//   lio::tick();
//
//   let result = shutdown_recv.try_recv().unwrap();
//   assert!(result.is_err(), "Shutdown on invalid fd should fail");
// }

#[test]
fn test_shutdown_after_close() {
  lio::init();

  let server_sock = lio::test_utils::tcp_socket().send();

  lio::tick();

  let server_sock = server_sock.recv().expect("Failed to create server socket");

  let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
  let bind_recv = api::bind(&server_sock, addr).send();

  lio::tick();

  bind_recv.recv().expect("Failed to bind");

  let bound_addr = unsafe {
    let mut addr_storage = MaybeUninit::<libc::sockaddr_in>::zeroed();
    let mut addr_len =
      std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
    libc::getsockname(
      server_sock.as_raw_fd(),
      addr_storage.as_mut_ptr() as *mut libc::sockaddr,
      &mut addr_len,
    );
    let sockaddr_in = addr_storage.assume_init();
    let port = u16::from_be(sockaddr_in.sin_port);
    format!("127.0.0.1:{}", port).parse::<SocketAddr>().unwrap()
  };

  let mut listen_recv = api::listen(&server_sock, 128).send();

  lio::tick();

  listen_recv.recv().expect("Failed to listen");

  let mut client_sock = lio::test_utils::tcp_socket().send();

  lio::tick();

  let client_sock = client_sock.recv().unwrap();

  let connect_recv = api::connect(&client_sock, bound_addr).send();

  lio::tick();

  connect_recv.recv().unwrap();

  // Close the socket
  unsafe {
    libc::close(client_sock.as_raw_fd());
  }

  // Try to shutdown after close (should fail)
  let shutdown_recv = api::shutdown(&client_sock, libc::SHUT_RDWR).send();

  lio::tick();

  let result = shutdown_recv.recv();
  assert!(result.is_err(), "Shutdown after close should fail");

  // Cleanup
  unsafe {
    libc::close(server_sock.as_raw_fd());
  }
}

#[test]
fn test_shutdown_twice() {
  lio::init();

  let server_sock = lio::test_utils::tcp_socket().send();

  lio::tick();

  let server_sock = server_sock.recv().expect("Failed to create server socket");

  let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
  let bind_recv = api::bind(&server_sock, addr).send();

  lio::tick();

  bind_recv.recv().expect("Failed to bind");

  let bound_addr = unsafe {
    let mut addr_storage = MaybeUninit::<libc::sockaddr_in>::zeroed();
    let mut addr_len =
      std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
    libc::getsockname(
      server_sock.as_raw_fd(),
      addr_storage.as_mut_ptr() as *mut libc::sockaddr,
      &mut addr_len,
    );
    let sockaddr_in = addr_storage.assume_init();
    let port = u16::from_be(sockaddr_in.sin_port);
    format!("127.0.0.1:{}", port).parse::<SocketAddr>().unwrap()
  };

  let mut listen_recv = api::listen(&server_sock, 128).send();

  lio::tick();

  listen_recv.recv().expect("Failed to listen");

  // Create client socket
  let mut client_sock = lio::test_utils::tcp_socket().send();

  lio::tick();

  let client_sock = client_sock.recv().unwrap();

  // Connect client to server
  let mut connect_recv = api::connect(&client_sock, bound_addr).send();

  lio::tick();

  connect_recv.recv().unwrap();

  // Accept connection on server
  let mut accept_recv = api::accept(&server_sock).send();

  lio::tick();
  lio::tick();

  let (server_client_fd, _addr) = accept_recv.recv().unwrap();

  // First shutdown
  let mut shutdown_recv = api::shutdown(&client_sock, libc::SHUT_WR).send();

  lio::tick();

  shutdown_recv.recv().expect("First shutdown should succeed");

  // Second shutdown on same direction
  let mut shutdown_recv2 = api::shutdown(&client_sock, libc::SHUT_WR).send();

  lio::tick();

  let result = shutdown_recv2.recv();
  // Some systems allow this, some don't - just verify it doesn't crash
  let _ = result;

  // Cleanup
  unsafe {
    libc::close(client_sock.as_raw_fd());
    libc::close(server_client_fd.as_raw_fd());
    libc::close(server_sock.as_raw_fd());
  }
}

#[test]
fn test_shutdown_sequential_directions() {
  lio::init();

  let mut server_sock = lio::test_utils::tcp_socket().send();

  lio::tick();

  let server_sock = server_sock.recv().unwrap();

  let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
  let mut bind_recv = api::bind(&server_sock, addr).send();

  lio::tick();

  bind_recv.recv().expect("Failed to bind");

  let bound_addr = unsafe {
    let mut addr_storage = MaybeUninit::<libc::sockaddr_in>::zeroed();
    let mut addr_len =
      std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
    libc::getsockname(
      server_sock.as_raw_fd(),
      addr_storage.as_mut_ptr() as *mut libc::sockaddr,
      &mut addr_len,
    );
    let sockaddr_in = addr_storage.assume_init();
    let port = u16::from_be(sockaddr_in.sin_port);
    format!("127.0.0.1:{}", port).parse::<SocketAddr>().unwrap()
  };

  let mut listen_recv = api::listen(&server_sock, 128).send();

  lio::tick();

  listen_recv.recv().expect("Failed to listen");

  // Create client socket
  let mut client_sock = lio::test_utils::tcp_socket().send();

  lio::tick();

  let client_sock = client_sock.recv().unwrap();

  // Connect client to server
  let mut connect_recv = api::connect(&client_sock, bound_addr).send();

  lio::tick();

  connect_recv.recv().unwrap();

  // Accept connection on server
  let mut accept_recv = api::accept(&server_sock).send();

  lio::tick();
  lio::tick();

  let (server_client_fd, _addr) = accept_recv.recv().unwrap();

  // Shutdown write first
  let mut shutdown_recv = api::shutdown(&client_sock, libc::SHUT_WR).send();

  lio::tick();

  shutdown_recv.recv().expect("Failed to shutdown write");

  // Client can still receive - send from server
  let data = b"From server".to_vec();
  let mut send_recv = api::send(&server_client_fd, data, None).send();

  lio::tick();

  let (bytes_sent, _) = send_recv.recv();
  assert!(bytes_sent.is_ok(), "Server send should succeed");

  // Receive on client
  let buf = vec![0u8; 100];
  let recv_recv = api::recv(&client_sock, buf, None).send();

  lio::tick();

  let (bytes_received, received_buf) = recv_recv.recv();
  let bytes_received = bytes_received.unwrap() as usize;
  assert_eq!(&received_buf[..bytes_received], b"From server");

  // Now shutdown read
  let shutdown_recv2 = api::shutdown(&client_sock, libc::SHUT_RD).send();

  lio::tick();

  shutdown_recv2.recv().expect("Failed to shutdown read");

  // Cleanup
  unsafe {
    libc::close(client_sock.as_raw_fd());
    libc::close(server_client_fd.as_raw_fd());
    libc::close(server_sock.as_raw_fd());
  }
}

#[test]
fn test_shutdown_before_data_sent() {
  lio::init();

  // Create server socket
  let server_sock = lio::test_utils::tcp_socket().send();

  lio::tick();

  let server_sock = server_sock.recv().expect("Failed to create server socket");

  let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
  let mut bind_recv = api::bind(&server_sock, addr).send();

  lio::tick();

  bind_recv.recv().expect("Failed to bind");

  let bound_addr = unsafe {
    let mut addr_storage = MaybeUninit::<libc::sockaddr_in>::zeroed();
    let mut addr_len =
      std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
    libc::getsockname(
      server_sock.as_raw_fd(),
      addr_storage.as_mut_ptr() as *mut libc::sockaddr,
      &mut addr_len,
    );
    let sockaddr_in = addr_storage.assume_init();
    let port = u16::from_be(sockaddr_in.sin_port);
    format!("127.0.0.1:{}", port).parse::<SocketAddr>().unwrap()
  };

  let mut listen_recv = api::listen(&server_sock, 128).send();

  lio::tick();

  listen_recv.recv().expect("Failed to listen");

  // Create client socket
  let mut client_sock = lio::test_utils::tcp_socket().send();

  lio::tick();

  let client_sock = client_sock.recv().unwrap();

  // Connect client to server
  let mut connect_recv = api::connect(&client_sock, bound_addr).send();

  lio::tick();

  connect_recv.recv().unwrap();

  // Accept connection on server
  let mut accept_recv = api::accept(&server_sock).send();

  lio::tick();
  lio::tick();

  let (server_client_fd, _addr) = accept_recv.recv().unwrap();

  // Shutdown immediately after connection, before any data transfer
  let mut shutdown_recv = api::shutdown(&client_sock, libc::SHUT_RDWR).send();

  lio::tick();

  shutdown_recv.recv().expect("Shutdown should succeed on fresh connection");

  // Verify server sees EOF
  let buf = vec![0u8; 100];
  let mut recv_recv = api::recv(&server_client_fd, buf, None).send();

  lio::tick();

  let (bytes_received, _) = recv_recv.recv();
  assert_eq!(bytes_received.expect("Recv should succeed"), 0);

  // Cleanup
  unsafe {
    libc::close(client_sock.as_raw_fd());
    libc::close(server_client_fd.as_raw_fd());
    libc::close(server_sock.as_raw_fd());
  }
}

#[test]
fn test_shutdown_ipv6() {
  lio::init();

  let server_sock = lio::test_utils::tcp6_socket().send();

  lio::tick();

  let server_sock =
    server_sock.recv().expect("Failed to create IPv6 server socket");

  let addr: SocketAddr = "[::1]:0".parse().unwrap();
  let mut bind_recv = api::bind(&server_sock, addr).send();

  lio::tick();

  bind_recv.recv().expect("Failed to bind IPv6");

  let bound_addr = unsafe {
    let mut addr_storage = MaybeUninit::<libc::sockaddr_in6>::zeroed();
    let mut addr_len =
      std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t;
    libc::getsockname(
      server_sock.as_raw_fd(),
      addr_storage.as_mut_ptr() as *mut libc::sockaddr,
      &mut addr_len,
    );
    let sockaddr_in6 = addr_storage.assume_init();
    let port = u16::from_be(sockaddr_in6.sin6_port);
    format!("[::1]:{}", port).parse::<SocketAddr>().unwrap()
  };

  let listen_recv = api::listen(&server_sock, 128).send();

  lio::tick();

  listen_recv.recv().expect("Failed to listen");

  // Create client socket
  let client_sock = lio::test_utils::tcp6_socket().send();

  lio::tick();

  let client_sock =
    client_sock.recv().expect("Failed to create IPv6 client socket");

  // Connect client to server
  let connect_recv = api::connect(&client_sock, bound_addr).send();

  lio::tick();

  connect_recv.recv().expect("Failed to connect IPv6");

  // Accept connection on server
  let mut accept_recv = api::accept(&server_sock).send();

  lio::tick();
  lio::tick();

  let (server_client_fd, _addr) =
    accept_recv.recv().expect("Failed to accept IPv6");

  // Shutdown write on IPv6 socket
  let mut shutdown_recv = api::shutdown(&client_sock, libc::SHUT_WR).send();

  lio::tick();

  shutdown_recv.recv().expect("Failed to shutdown IPv6 socket");

  // Verify EOF
  let buf = vec![0u8; 100];
  let mut recv_recv = api::recv(&server_client_fd, buf, None).send();

  lio::tick();

  let (bytes_received, _) = recv_recv.recv();
  assert_eq!(bytes_received.expect("Recv should succeed"), 0);

  // Cleanup
  unsafe {
    libc::close(client_sock.as_raw_fd());
    libc::close(server_client_fd.as_raw_fd());
    libc::close(server_sock.as_raw_fd());
  }
}

#[test]
fn test_shutdown_concurrent() {
  lio::init();

  // Test shutting down multiple connections (sequentially)
  for _ in 0..5 {
    let mut server_sock = lio::test_utils::tcp_socket().send();

    lio::tick();

    let server_sock =
      server_sock.recv().expect("Failed to create server socket");

    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let mut bind_recv = api::bind(&server_sock, addr).send();

    lio::tick();

    bind_recv.recv().expect("Failed to bind");

    let bound_addr = unsafe {
      let mut addr_storage = MaybeUninit::<libc::sockaddr_in>::zeroed();
      let mut addr_len =
        std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
      libc::getsockname(
        server_sock.as_raw_fd(),
        addr_storage.as_mut_ptr() as *mut libc::sockaddr,
        &mut addr_len,
      );
      let sockaddr_in = addr_storage.assume_init();
      let port = u16::from_be(sockaddr_in.sin_port);
      format!("127.0.0.1:{}", port).parse::<SocketAddr>().unwrap()
    };

    let mut listen_recv = api::listen(&server_sock, 128).send();

    lio::tick();

    listen_recv.recv().expect("Failed to listen");

    // Create and connect client
    let client_sock = lio::test_utils::tcp_socket().send();

    lio::tick();

    let client_sock = client_sock.recv().unwrap();

    let connect_recv = api::connect(&client_sock, bound_addr).send();

    lio::tick();

    connect_recv.recv().unwrap();

    // Accept connection
    let accept_recv = api::accept(&server_sock).send();

    lio::tick();
    lio::tick();

    let (server_client_fd, _addr) = accept_recv.recv().unwrap();

    // Shutdown
    let mut shutdown_recv = api::shutdown(&client_sock, libc::SHUT_RDWR).send();

    lio::tick();

    shutdown_recv.recv().expect("Concurrent shutdown failed");

    unsafe {
      libc::close(client_sock.as_raw_fd());
      libc::close(server_client_fd.as_raw_fd());
      libc::close(server_sock.as_raw_fd());
    }
  }
}

#[test]
#[ignore = "flaky shutdown test"]
fn test_shutdown_with_pending_data() {
  lio::init();

  let mut server_sock = lio::test_utils::tcp_socket().send();

  lio::tick();

  let server_sock = server_sock.recv().unwrap();

  let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
  let mut bind_recv = api::bind(&server_sock, addr).send();

  lio::tick();

  bind_recv.recv().expect("Failed to bind");

  let bound_addr = unsafe {
    let mut addr_storage = MaybeUninit::<libc::sockaddr_in>::zeroed();
    let mut addr_len =
      std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
    libc::getsockname(
      server_sock.as_raw_fd(),
      addr_storage.as_mut_ptr() as *mut libc::sockaddr,
      &mut addr_len,
    );
    let sockaddr_in = addr_storage.assume_init();
    let port = u16::from_be(sockaddr_in.sin_port);
    format!("127.0.0.1:{}", port).parse::<SocketAddr>().unwrap()
  };

  let mut listen_recv = api::listen(&server_sock, 128).send();

  lio::tick();

  listen_recv.recv().expect("Failed to listen");

  // Create client socket
  let mut client_sock = lio::test_utils::tcp_socket().send();

  lio::tick();

  let client_sock = client_sock.recv().unwrap();

  // Connect client to server
  let mut connect_recv = api::connect(&client_sock, bound_addr).send();

  lio::tick();

  connect_recv.recv().unwrap();

  // Accept connection on server
  let mut accept_recv = api::accept(&server_sock).send();

  lio::tick();
  lio::tick();

  let (server_client_fd, _addr) = accept_recv.recv().unwrap();

  // Send some data from client
  let data = b"Data before shutdown".to_vec();
  let mut send_recv = api::send(&client_sock, data.clone(), None).send();

  lio::tick();

  let (bytes_sent, _) = send_recv.recv();
  assert!(bytes_sent.is_ok(), "Send should succeed");

  // Shutdown write immediately (data may still be in transit)
  let mut shutdown_recv = api::shutdown(&client_sock, libc::SHUT_WR).send();

  lio::tick();

  shutdown_recv.recv().expect("Shutdown should succeed");

  // Server should still be able to receive the data
  let buf = vec![0u8; 100];
  let mut recv_recv = api::recv(&server_client_fd, buf, None).send();

  lio::tick();

  let (bytes_received, received_buf) = recv_recv.recv();
  let bytes_received = bytes_received.expect("Recv should succeed") as usize;

  // Should receive the data followed by EOF
  if bytes_received > 0 {
    assert_eq!(&received_buf[..bytes_received], data.as_slice());

    // Next read should be EOF
    let buf2 = vec![0u8; 100];
    let mut recv_recv2 = api::recv(&server_client_fd, buf2, None).send();

    lio::tick();

    let (bytes_received2, _) = recv_recv2.recv();
    assert_eq!(bytes_received2.expect("Recv should succeed"), 0);
  }

  // Cleanup
  unsafe {
    libc::close(client_sock.as_raw_fd());
    libc::close(server_client_fd.as_raw_fd());
    libc::close(server_sock.as_raw_fd());
  }
  lio::exit();
}
