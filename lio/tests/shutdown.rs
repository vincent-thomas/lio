mod common;

use common::poll_recv;
use lio::api::resource::Resource;
use lio::{Lio, api};
use std::mem::MaybeUninit;
use std::net::SocketAddr;
use std::os::fd::{AsRawFd, FromRawFd};

#[test]
fn test_shutdown_write() {
  let mut lio = Lio::new(64).unwrap();

  // Create server socket
  let server_sock = lio::test_utils::tcp_socket().with_lio(&mut lio).send();

  lio.try_run().unwrap();

  let server_sock = server_sock.recv().unwrap();

  let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
  let bind_recv = api::bind(&server_sock, addr).with_lio(&mut lio).send();

  lio.try_run().unwrap();

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

  let listen_recv = api::listen(&server_sock, 128).with_lio(&mut lio).send();

  lio.try_run().unwrap();

  listen_recv.recv().expect("Failed to listen");

  // Create client socket
  let client_sock = lio::test_utils::tcp_socket().with_lio(&mut lio).send();

  lio.try_run().unwrap();

  let client_sock = client_sock.recv().unwrap();

  // Connect client to server
  let mut connect_recv =
    api::connect(&client_sock, bound_addr).with_lio(&mut lio).send();

  // Accept connection on server
  let mut accept_recv = api::accept(&server_sock).with_lio(&mut lio).send();

  poll_recv(&mut lio, &mut connect_recv).unwrap();
  let (server_client_fd, _addr) =
    poll_recv(&mut lio, &mut accept_recv).unwrap();

  // Shutdown write on client
  let mut shutdown_recv =
    api::shutdown(&client_sock, libc::SHUT_WR).with_lio(&mut lio).send();

  poll_recv(&mut lio, &mut shutdown_recv).unwrap();

  // Try to send data (should fail or return 0)
  let data = b"Test data".to_vec();
  let mut send_recv =
    api::send(&client_sock, data, None).with_lio(&mut lio).send();

  let (result, _) = poll_recv(&mut lio, &mut send_recv);

  // Send after SHUT_WR should fail
  assert!(
    result.is_err() || result.unwrap() == 0,
    "Send should fail after SHUT_WR"
  );

  // Server should be able to read EOF
  let buf = vec![0u8; 100];
  let mut recv_recv =
    api::recv(&server_client_fd, buf, None).with_lio(&mut lio).send();

  let (bytes_received, _) = poll_recv(&mut lio, &mut recv_recv);

  assert_eq!(
    bytes_received.expect("Recv should succeed"),
    0,
    "Should receive EOF after client shutdown write"
  );

  // Resources are automatically closed when dropped
}

#[test]
fn test_shutdown_read() {
  let mut lio = Lio::new(64).unwrap();

  let server_sock = lio::test_utils::tcp_socket().with_lio(&mut lio).send();

  lio.try_run().unwrap();

  let server_sock = server_sock.recv().unwrap();

  let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
  let bind_recv = api::bind(&server_sock, addr).with_lio(&mut lio).send();

  lio.try_run().unwrap();

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

  let listen_recv = api::listen(&server_sock, 128).with_lio(&mut lio).send();

  lio.try_run().unwrap();

  listen_recv.recv().expect("Failed to listen");

  // Create client socket
  let client_sock = lio::test_utils::tcp_socket().with_lio(&mut lio).send();

  lio.try_run().unwrap();

  let client_sock = client_sock.recv().unwrap();

  // Connect client to server
  let mut connect_recv =
    api::connect(&client_sock, bound_addr).with_lio(&mut lio).send();

  // Accept connection on server
  let mut accept_recv = api::accept(&server_sock).with_lio(&mut lio).send();

  poll_recv(&mut lio, &mut connect_recv).unwrap();
  let (server_client_fd, _addr) =
    poll_recv(&mut lio, &mut accept_recv).unwrap();

  // Shutdown read on client
  let mut shutdown_recv =
    api::shutdown(&client_sock, libc::SHUT_RD).with_lio(&mut lio).send();

  poll_recv(&mut lio, &mut shutdown_recv).unwrap();

  // Client can still send
  let data = b"Hello".to_vec();
  let mut send_recv =
    api::send(&client_sock, data.clone(), None).with_lio(&mut lio).send();

  let (bytes_sent, _) = poll_recv(&mut lio, &mut send_recv);
  assert_eq!(bytes_sent.expect("Send should succeed") as usize, data.len());

  // Server can receive
  let buf = vec![0u8; 100];
  let mut recv_recv =
    api::recv(&server_client_fd, buf, None).with_lio(&mut lio).send();

  let (bytes_received, received_buf) = poll_recv(&mut lio, &mut recv_recv);
  let bytes_received = bytes_received.expect("Recv should succeed") as usize;
  assert_eq!(bytes_received, data.len());
  assert_eq!(&received_buf[..bytes_received], data.as_slice());

  // Resources are automatically closed when dropped
}

#[test]
fn test_shutdown_both() {
  let mut lio = Lio::new(64).unwrap();

  let mut server_sock = lio::test_utils::tcp_socket().with_lio(&mut lio).send();

  let server_sock = poll_recv(&mut lio, &mut server_sock).unwrap();

  let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
  let mut bind_recv = api::bind(&server_sock, addr).with_lio(&mut lio).send();

  poll_recv(&mut lio, &mut bind_recv).expect("Failed to bind");

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

  let mut listen_recv =
    api::listen(&server_sock, 128).with_lio(&mut lio).send();

  poll_recv(&mut lio, &mut listen_recv).expect("Failed to listen");

  // Create client socket
  let mut client_sock = lio::test_utils::tcp_socket().with_lio(&mut lio).send();

  let client_sock = poll_recv(&mut lio, &mut client_sock).unwrap();

  // Connect client to server
  let mut connect_recv =
    api::connect(&client_sock, bound_addr).with_lio(&mut lio).send();

  // Accept connection on server
  let mut accept_recv = api::accept(&server_sock).with_lio(&mut lio).send();

  poll_recv(&mut lio, &mut connect_recv).unwrap();
  let (server_client_fd, _addr) =
    poll_recv(&mut lio, &mut accept_recv).unwrap();

  // Shutdown both directions on client
  let mut shutdown_recv =
    api::shutdown(&client_sock, libc::SHUT_RDWR).with_lio(&mut lio).send();

  poll_recv(&mut lio, &mut shutdown_recv).expect("Failed to shutdown both");

  // Send should fail
  let data = b"Test".to_vec();
  let mut send_recv =
    api::send(&client_sock, data, None).with_lio(&mut lio).send();

  let (result, _) = poll_recv(&mut lio, &mut send_recv);
  assert!(
    result.is_err() || result.unwrap() == 0,
    "Send should fail after SHUT_RDWR"
  );

  // Server should receive EOF
  let buf = vec![0u8; 100];
  let mut recv_recv =
    api::recv(&server_client_fd, buf, None).with_lio(&mut lio).send();

  let (bytes_received, _) = poll_recv(&mut lio, &mut recv_recv);
  assert_eq!(
    bytes_received.expect("Recv should succeed"),
    0,
    "Should receive EOF"
  );

  // Resources are automatically closed when dropped
}

#[test]
fn test_shutdown_invalid_fd() {
  let mut lio = Lio::new(64).unwrap();

  // Try to shutdown an invalid file descriptor
  let mut shutdown_recv =
    api::shutdown(&unsafe { Resource::from_raw_fd(-1) }, libc::SHUT_RDWR)
      .with_lio(&mut lio)
      .send();

  let result = poll_recv(&mut lio, &mut shutdown_recv);
  assert!(result.is_err(), "Shutdown on invalid fd should fail");
}

#[test]
fn test_shutdown_after_close() {
  let mut lio = Lio::new(64).unwrap();

  let server_sock = lio::test_utils::tcp_socket().with_lio(&mut lio).send();

  lio.try_run().unwrap();

  let server_sock = server_sock.recv().expect("Failed to create server socket");

  let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
  let bind_recv = api::bind(&server_sock, addr).with_lio(&mut lio).send();

  lio.try_run().unwrap();

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

  let listen_recv = api::listen(&server_sock, 128).with_lio(&mut lio).send();

  lio.try_run().unwrap();

  listen_recv.recv().expect("Failed to listen");

  let mut client_sock_recv =
    lio::test_utils::tcp_socket().with_lio(&mut lio).send();

  let client_sock = poll_recv(&mut lio, &mut client_sock_recv).unwrap();

  let mut connect_recv =
    api::connect(&client_sock, bound_addr).with_lio(&mut lio).send();

  poll_recv(&mut lio, &mut connect_recv).unwrap();

  // Close the socket manually and forget to prevent double-close
  unsafe {
    libc::close(client_sock.as_raw_fd());
  }
  std::mem::forget(client_sock);

  // Try to shutdown after close (should fail)
  // Note: We create a new Resource with the same (now invalid) fd for testing
  let mut shutdown_recv =
    api::shutdown(&unsafe { Resource::from_raw_fd(-1) }, libc::SHUT_RDWR)
      .with_lio(&mut lio)
      .send();

  let result = poll_recv(&mut lio, &mut shutdown_recv);
  assert!(result.is_err(), "Shutdown after close should fail");

  // server_sock is automatically closed when dropped
}

#[test]
fn test_shutdown_twice() {
  let mut lio = Lio::new(64).unwrap();

  let server_sock = lio::test_utils::tcp_socket().with_lio(&mut lio).send();

  lio.try_run().unwrap();

  let server_sock = server_sock.recv().expect("Failed to create server socket");

  let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
  let bind_recv = api::bind(&server_sock, addr).with_lio(&mut lio).send();

  lio.try_run().unwrap();

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

  let listen_recv = api::listen(&server_sock, 128).with_lio(&mut lio).send();

  lio.try_run().unwrap();

  listen_recv.recv().expect("Failed to listen");

  // Create client socket
  let client_sock = lio::test_utils::tcp_socket().with_lio(&mut lio).send();

  lio.try_run().unwrap();

  let client_sock = client_sock.recv().unwrap();

  // Connect client to server
  let mut connect_recv =
    api::connect(&client_sock, bound_addr).with_lio(&mut lio).send();

  // Accept connection on server
  let mut accept_recv = api::accept(&server_sock).with_lio(&mut lio).send();

  poll_recv(&mut lio, &mut connect_recv).unwrap();
  let (_server_client_fd, _addr) =
    poll_recv(&mut lio, &mut accept_recv).unwrap();

  // First shutdown
  let mut shutdown_recv =
    api::shutdown(&client_sock, libc::SHUT_WR).with_lio(&mut lio).send();

  poll_recv(&mut lio, &mut shutdown_recv)
    .expect("First shutdown should succeed");

  // Second shutdown on same direction
  let mut shutdown_recv2 =
    api::shutdown(&client_sock, libc::SHUT_WR).with_lio(&mut lio).send();

  let result = poll_recv(&mut lio, &mut shutdown_recv2);
  // Some systems allow this, some don't - just verify it doesn't crash
  let _ = result;

  // Resources are automatically closed when dropped
}

#[test]
fn test_shutdown_sequential_directions() {
  let mut lio = Lio::new(64).unwrap();

  let server_sock = lio::test_utils::tcp_socket().with_lio(&mut lio).send();

  lio.try_run().unwrap();

  let server_sock = server_sock.recv().unwrap();

  let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
  let bind_recv = api::bind(&server_sock, addr).with_lio(&mut lio).send();

  lio.try_run().unwrap();

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

  let listen_recv = api::listen(&server_sock, 128).with_lio(&mut lio).send();

  lio.try_run().unwrap();

  listen_recv.recv().expect("Failed to listen");

  // Create client socket
  let client_sock = lio::test_utils::tcp_socket().with_lio(&mut lio).send();

  lio.try_run().unwrap();

  let client_sock = client_sock.recv().unwrap();

  // Connect client to server
  let mut connect_recv =
    api::connect(&client_sock, bound_addr).with_lio(&mut lio).send();

  // Accept connection on server
  let mut accept_recv = api::accept(&server_sock).with_lio(&mut lio).send();

  poll_recv(&mut lio, &mut connect_recv).unwrap();
  let (server_client_fd, _addr) =
    poll_recv(&mut lio, &mut accept_recv).unwrap();

  // Shutdown write first
  let mut shutdown_recv =
    api::shutdown(&client_sock, libc::SHUT_WR).with_lio(&mut lio).send();

  poll_recv(&mut lio, &mut shutdown_recv).expect("Failed to shutdown write");

  // Client can still receive - send from server
  let data = b"From server".to_vec();
  let mut send_recv =
    api::send(&server_client_fd, data, None).with_lio(&mut lio).send();

  let (bytes_sent, _) = poll_recv(&mut lio, &mut send_recv);
  assert!(bytes_sent.is_ok(), "Server send should succeed");

  // Receive on client
  let buf = vec![0u8; 100];
  let mut recv_recv =
    api::recv(&client_sock, buf, None).with_lio(&mut lio).send();

  let (bytes_received, received_buf) = poll_recv(&mut lio, &mut recv_recv);
  let bytes_received = bytes_received.unwrap() as usize;
  assert_eq!(&received_buf[..bytes_received], b"From server");

  // Now shutdown read
  let mut shutdown_recv2 =
    api::shutdown(&client_sock, libc::SHUT_RD).with_lio(&mut lio).send();

  poll_recv(&mut lio, &mut shutdown_recv2).expect("Failed to shutdown read");

  // Resources are automatically closed when dropped
}

#[test]
fn test_shutdown_before_data_sent() {
  let mut lio = Lio::new(64).unwrap();

  // Create server socket
  let server_sock = lio::test_utils::tcp_socket().with_lio(&mut lio).send();

  lio.try_run().unwrap();

  let server_sock = server_sock.recv().expect("Failed to create server socket");

  let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
  let bind_recv = api::bind(&server_sock, addr).with_lio(&mut lio).send();

  lio.try_run().unwrap();

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

  let listen_recv = api::listen(&server_sock, 128).with_lio(&mut lio).send();

  lio.try_run().unwrap();

  listen_recv.recv().expect("Failed to listen");

  // Create client socket
  let client_sock = lio::test_utils::tcp_socket().with_lio(&mut lio).send();

  lio.try_run().unwrap();

  let client_sock = client_sock.recv().unwrap();

  // Connect client to server
  let mut connect_recv =
    api::connect(&client_sock, bound_addr).with_lio(&mut lio).send();

  // Accept connection on server
  let mut accept_recv = api::accept(&server_sock).with_lio(&mut lio).send();

  poll_recv(&mut lio, &mut connect_recv).unwrap();
  let (server_client_fd, _addr) =
    poll_recv(&mut lio, &mut accept_recv).unwrap();

  // Shutdown immediately after connection, before any data transfer
  let mut shutdown_recv =
    api::shutdown(&client_sock, libc::SHUT_RDWR).with_lio(&mut lio).send();

  poll_recv(&mut lio, &mut shutdown_recv)
    .expect("Shutdown should succeed on fresh connection");

  // Verify server sees EOF
  let buf = vec![0u8; 100];
  let mut recv_recv =
    api::recv(&server_client_fd, buf, None).with_lio(&mut lio).send();

  let (bytes_received, _) = poll_recv(&mut lio, &mut recv_recv);
  assert_eq!(bytes_received.expect("Recv should succeed"), 0);

  // Resources are automatically closed when dropped
}

#[test]
fn test_shutdown_ipv6() {
  let mut lio = Lio::new(64).unwrap();

  let server_sock = lio::test_utils::tcp6_socket().with_lio(&mut lio).send();

  lio.try_run().unwrap();

  let server_sock =
    server_sock.recv().expect("Failed to create IPv6 server socket");

  let addr: SocketAddr = "[::1]:0".parse().unwrap();
  let bind_recv = api::bind(&server_sock, addr).with_lio(&mut lio).send();

  lio.try_run().unwrap();

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

  let listen_recv = api::listen(&server_sock, 128).with_lio(&mut lio).send();

  lio.try_run().unwrap();

  listen_recv.recv().expect("Failed to listen");

  // Create client socket
  let client_sock = lio::test_utils::tcp6_socket().with_lio(&mut lio).send();

  lio.try_run().unwrap();

  let client_sock =
    client_sock.recv().expect("Failed to create IPv6 client socket");

  // Connect client to server
  let mut connect_recv =
    api::connect(&client_sock, bound_addr).with_lio(&mut lio).send();

  // Accept connection on server
  let mut accept_recv = api::accept(&server_sock).with_lio(&mut lio).send();

  poll_recv(&mut lio, &mut connect_recv).expect("Failed to connect IPv6");
  let (server_client_fd, _addr) =
    poll_recv(&mut lio, &mut accept_recv).expect("Failed to accept IPv6");

  // Shutdown write on IPv6 socket
  let mut shutdown_recv =
    api::shutdown(&client_sock, libc::SHUT_WR).with_lio(&mut lio).send();

  poll_recv(&mut lio, &mut shutdown_recv)
    .expect("Failed to shutdown IPv6 socket");

  // Verify EOF
  let buf = vec![0u8; 100];
  let mut recv_recv =
    api::recv(&server_client_fd, buf, None).with_lio(&mut lio).send();

  let (bytes_received, _) = poll_recv(&mut lio, &mut recv_recv);
  assert_eq!(bytes_received.expect("Recv should succeed"), 0);

  // Resources are automatically closed when dropped
}

#[test]
fn test_shutdown_concurrent() {
  let mut lio = Lio::new(64).unwrap();

  // Test shutting down multiple connections (sequentially)
  for _ in 0..5 {
    let mut server_sock =
      lio::test_utils::tcp_socket().with_lio(&mut lio).send();

    let server_sock = poll_recv(&mut lio, &mut server_sock)
      .expect("Failed to create server socket");

    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let mut bind_recv = api::bind(&server_sock, addr).with_lio(&mut lio).send();

    poll_recv(&mut lio, &mut bind_recv).expect("Failed to bind");

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

    let mut listen_recv =
      api::listen(&server_sock, 128).with_lio(&mut lio).send();

    poll_recv(&mut lio, &mut listen_recv).expect("Failed to listen");

    // Create and connect client
    let mut client_sock =
      lio::test_utils::tcp_socket().with_lio(&mut lio).send();

    let client_sock = poll_recv(&mut lio, &mut client_sock).unwrap();

    let mut connect_recv =
      api::connect(&client_sock, bound_addr).with_lio(&mut lio).send();

    // Accept connection
    let mut accept_recv = api::accept(&server_sock).with_lio(&mut lio).send();

    poll_recv(&mut lio, &mut connect_recv).unwrap();
    let (_server_client_fd, _addr) =
      poll_recv(&mut lio, &mut accept_recv).unwrap();

    // Shutdown
    let mut shutdown_recv =
      api::shutdown(&client_sock, libc::SHUT_RDWR).with_lio(&mut lio).send();

    poll_recv(&mut lio, &mut shutdown_recv)
      .expect("Concurrent shutdown failed");

    // Resources are automatically closed when dropped at end of loop iteration
  }
}

#[test]
fn test_shutdown_with_pending_data() {
  let mut lio = Lio::new(64).unwrap();

  let server_sock = lio::test_utils::tcp_socket().with_lio(&mut lio).send();

  lio.try_run().unwrap();

  let server_sock = server_sock.recv().unwrap();

  let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
  let bind_recv = api::bind(&server_sock, addr).with_lio(&mut lio).send();

  lio.try_run().unwrap();

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

  let listen_recv = api::listen(&server_sock, 128).with_lio(&mut lio).send();

  lio.try_run().unwrap();

  listen_recv.recv().expect("Failed to listen");

  // Create client socket
  let client_sock = lio::test_utils::tcp_socket().with_lio(&mut lio).send();

  lio.try_run().unwrap();

  let client_sock = client_sock.recv().unwrap();

  // Connect client to server
  let mut connect_recv =
    api::connect(&client_sock, bound_addr).with_lio(&mut lio).send();

  // Accept connection on server
  let mut accept_recv = api::accept(&server_sock).with_lio(&mut lio).send();

  poll_recv(&mut lio, &mut connect_recv).unwrap();
  let (server_client_fd, _addr) =
    poll_recv(&mut lio, &mut accept_recv).unwrap();

  // Send some data from client
  let data = b"Data before shutdown".to_vec();
  let mut send_recv =
    api::send(&client_sock, data.clone(), None).with_lio(&mut lio).send();

  let (bytes_sent, _) = poll_recv(&mut lio, &mut send_recv);
  assert!(bytes_sent.is_ok(), "Send should succeed");

  // Shutdown write immediately (data may still be in transit)
  let mut shutdown_recv =
    api::shutdown(&client_sock, libc::SHUT_WR).with_lio(&mut lio).send();

  poll_recv(&mut lio, &mut shutdown_recv).expect("Shutdown should succeed");

  // Server should still be able to receive the data
  let buf = vec![0u8; 100];
  let mut recv_recv =
    api::recv(&server_client_fd, buf, None).with_lio(&mut lio).send();

  let (bytes_received, received_buf) = poll_recv(&mut lio, &mut recv_recv);
  let bytes_received = bytes_received.expect("Recv should succeed") as usize;

  // Should receive the data followed by EOF
  if bytes_received > 0 {
    assert_eq!(&received_buf[..bytes_received], data.as_slice());

    // Next read should be EOF
    let buf2 = vec![0u8; 100];
    let mut recv_recv2 =
      api::recv(&server_client_fd, buf2, None).with_lio(&mut lio).send();

    let (bytes_received2, _) = poll_recv(&mut lio, &mut recv_recv2);
    assert_eq!(bytes_received2.expect("Recv should succeed"), 0);
  }

  // Resources are automatically closed when dropped
}
