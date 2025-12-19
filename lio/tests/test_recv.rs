use lio::{accept, bind, connect, listen, recv, send, socket};
use proptest::prelude::*;
use proptest::test_runner::{Config, TestRunner};
use socket2::{Domain, Protocol, Type};
use std::mem::MaybeUninit;
use std::net::SocketAddr;
use std::sync::mpsc::{self, TryRecvError};

#[ignore = "flaky network test"]
#[test]
#[ignore]
fn test_recv_multiple() {
  lio::init();

  let (sender_sock, receiver_sock) = mpsc::channel();
  let (sender_unit, receiver_unit) = mpsc::channel();

  // Create server socket
  let sender_s1 = sender_sock.clone();
  socket(Domain::IPV4, Type::STREAM, Some(Protocol::TCP)).when_done(
    move |res| {
      sender_s1.send(res).unwrap();
    },
  );

  // assert_eq!(receiver_sock.try_recv().unwrap_err(), TryRecvError::Empty);
  lio::tick();

  let server_sock =
    receiver_sock.recv().unwrap().expect("Failed to create server socket");

  let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();

  let sender_b = sender_unit.clone();
  bind(server_sock, addr).when_done(move |res| {
    sender_b.send(res).unwrap();
  });

  // assert_eq!(receiver_unit.try_recv().unwrap_err(), TryRecvError::Empty);
  lio::tick();

  receiver_unit.recv().unwrap().expect("Failed to bind");

  let bound_addr = unsafe {
    let mut addr_storage = MaybeUninit::<libc::sockaddr_in>::zeroed();
    let mut addr_len =
      std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
    libc::getsockname(
      server_sock,
      addr_storage.as_mut_ptr() as *mut libc::sockaddr,
      &mut addr_len,
    );
    let sockaddr_in = addr_storage.assume_init();
    let port = u16::from_be(sockaddr_in.sin_port);
    format!("127.0.0.1:{}", port).parse::<SocketAddr>().unwrap()
  };

  let sender_l = sender_unit.clone();
  listen(server_sock, 128).when_done(move |res| {
    sender_l.send(res).unwrap();
  });

  // assert_eq!(receiver_unit.try_recv().unwrap_err(), TryRecvError::Empty);
  lio::tick();

  receiver_unit.recv().unwrap().expect("Failed to listen");

  // Create client socket
  let sender_cs = sender_sock.clone();
  socket(Domain::IPV4, Type::STREAM, Some(Protocol::TCP)).when_done(
    move |res| {
      sender_cs.send(res).unwrap();
    },
  );

  // assert_eq!(receiver_sock.try_recv().unwrap_err(), TryRecvError::Empty);
  lio::tick();

  let client_sock =
    receiver_sock.recv().unwrap().expect("Failed to create client socket");

  // Accept and connect
  let (sender_a, receiver_a) = mpsc::channel();
  let (sender_c, receiver_c) = mpsc::channel();

  accept(server_sock).when_done(move |res| {
    sender_a.send(res).unwrap();
  });

  connect(client_sock, bound_addr).when_done(move |res| {
    sender_c.send(res).unwrap();
  });

  // assert_eq!(receiver_a.try_recv().unwrap_err(), TryRecvError::Empty);
  lio::tick();
  lio::tick();

  println!("About to recv accept/connect results");
  let (client_fd, _client_addr) =
    receiver_a.recv().unwrap().expect("Failed to accept");
  println!("Accept succeeded, client_fd = {}", client_fd);
  receiver_c.recv().unwrap().expect("Failed to connect");
  println!("Connect succeeded");

  // Send multiple messages from client
  let (sender_send, receiver_send) = mpsc::channel();
  println!("Queueing 3 send operations");
  for i in 0..3 {
    let data = format!("Message {}", i).into_bytes();
    send(client_sock, data, None).send_with(sender_send.clone());
  }

  // assert_eq!(receiver_send.try_recv().unwrap_err(), TryRecvError::Empty);
  println!("About to tick for sends");
  lio::tick();
  // lio::tick(); // Need extra tick for sends to complete on kqueue
  println!("Ticked");

  println!("About to recv send results");
  for iter in 0..3 {
    let (res, _buf) = receiver_send
      .try_recv()
      .expect(&format!("Failed to send, iter = {iter}"));
    res.expect(&format!("Failed to send, iter = {iter}"));
  }
  println!("All sends completed");

  // Shutdown write side to signal EOF to server
  unsafe {
    libc::shutdown(client_sock, libc::SHUT_WR);
  }

  lio::tick();

  // Receive until EOF
  let mut all_data = Vec::new();
  println!("Starting recv loop");
  loop {
    println!("Loop iteration, all_data.len() = {}", all_data.len());
    let buf = vec![0u8; 1024];
    let (sender_recv, receiver_recv) = mpsc::channel();
    recv(client_fd, buf, None).when_done(move |res| {
      sender_recv.send(res).unwrap();
    });

    // assert_eq!(receiver_recv.try_recv().unwrap_err(), TryRecvError::Empty);
    println!("About to tick");
    lio::tick();
    println!("Ticked");

    println!("About to try_recv");
    let (bytes_received, received_buf) = receiver_recv.try_recv().unwrap();
    println!("try_recv succeeded");

    let bytes_received = bytes_received.expect("Failed to receive") as usize;
    println!("Received {} bytes", bytes_received);

    if bytes_received == 0 {
      break; // EOF
    }

    all_data.extend_from_slice(&received_buf[..bytes_received]);
  }

  // Verify we received all 3 messages concatenated
  let expected = b"Message 0Message 1Message 2";
  assert_eq!(all_data, expected);

  // Cleanup
  let (sender_close, receiver_close) = mpsc::channel();

  let sender_close1 = sender_close.clone();
  lio::close(client_sock).when_done(move |res| {
    sender_close1.send(res).unwrap();
  });

  let sender_close2 = sender_close.clone();
  lio::close(client_fd).when_done(move |res| {
    sender_close2.send(res).unwrap();
  });

  let sender_close3 = sender_close.clone();
  lio::close(server_sock).when_done(move |res| {
    sender_close3.send(res).unwrap();
  });

  // assert_eq!(receiver_close.try_recv().unwrap_err(), TryRecvError::Empty);
  lio::tick();

  receiver_close.recv().unwrap().expect("Failed to close client");
  receiver_close.recv().unwrap().expect("Failed to close server client");
  receiver_close.recv().unwrap().expect("Failed to close server");
}

#[ignore = "flaky network test"]
#[test]
fn test_recv_with_flags() {
  lio::init();

  let (sender_sock, receiver_sock) = mpsc::channel();
  let (sender_unit, receiver_unit) = mpsc::channel();

  // Create server socket
  socket(Domain::IPV4, Type::STREAM, Some(Protocol::TCP))
    .send_with(sender_sock.clone());

  // assert_eq!(receiver_sock.try_recv().unwrap_err(), TryRecvError::Empty);
  lio::tick();

  let server_sock =
    receiver_sock.try_recv().unwrap().expect("Failed to create server socket");

  let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();

  let sender_b = sender_unit.clone();
  bind(server_sock, addr).send_with(sender_b.clone());

  lio::tick();

  receiver_unit.try_recv().unwrap().expect("Failed to bind");

  let bound_addr = unsafe {
    let mut addr_storage = MaybeUninit::<libc::sockaddr_in>::zeroed();
    let mut addr_len =
      std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
    libc::getsockname(
      server_sock,
      addr_storage.as_mut_ptr() as *mut libc::sockaddr,
      &mut addr_len,
    );
    let sockaddr_in = addr_storage.assume_init();
    let port = u16::from_be(sockaddr_in.sin_port);
    format!("127.0.0.1:{}", port).parse::<SocketAddr>().unwrap()
  };

  let sender_l = sender_unit.clone();
  listen(server_sock, 128).when_done(move |res| {
    sender_l.send(res).unwrap();
  });

  // assert_eq!(receiver_unit.try_recv().unwrap_err(), TryRecvError::Empty);
  lio::tick();

  receiver_unit.try_recv().unwrap().expect("Failed to listen");

  // Create client socket
  let sender_cs = sender_sock.clone();
  socket(Domain::IPV4, Type::STREAM, Some(Protocol::TCP)).when_done(
    move |res| {
      sender_cs.send(res).unwrap();
    },
  );

  // assert_eq!(receiver_sock.try_recv().unwrap_err(), TryRecvError::Empty);
  lio::tick();

  let client_sock =
    receiver_sock.try_recv().unwrap().expect("Failed to create client socket");

  // Accept and connect
  let (sender_a, receiver_a) = mpsc::channel();
  let (sender_c, receiver_c) = mpsc::channel();

  connect(client_sock, bound_addr).when_done(move |res| {
    sender_c.send(res).unwrap();
  });

  lio::tick();

  accept(server_sock).when_done(move |res| {
    sender_a.send(res).unwrap();
  });

  // assert_eq!(receiver_c.try_recv().unwrap_err(), TryRecvError::Empty);
  lio::tick();

  let (client_fd, _client_addr) =
    receiver_a.try_recv().unwrap().expect("Failed to accept");
  receiver_c.try_recv().unwrap().expect("Failed to connect");

  // Send data
  let send_data = b"Data with flags".to_vec();
  let (sender_send, receiver_send) = mpsc::channel();
  send(client_sock, send_data, None).when_done(move |res| {
    sender_send.send(res).unwrap();
  });

  assert_eq!(receiver_send.try_recv().unwrap_err(), TryRecvError::Empty);
  lio::tick();

  let (res, send_data) = receiver_send.try_recv().unwrap();
  res.expect("Failed to send");

  // Receive with flags
  let buf = vec![0u8; 1024];
  let (sender_recv, receiver_recv) = mpsc::channel();
  recv(client_fd, buf, Some(0)).when_done(move |res| {
    sender_recv.send(res).unwrap();
  });

  assert_eq!(receiver_recv.try_recv().unwrap_err(), TryRecvError::Empty);
  lio::tick();

  let (bytes_received, received_buf) = receiver_recv.try_recv().unwrap();
  let bytes_received = bytes_received.expect("Failed to receive with flags");

  assert_eq!(bytes_received as usize, send_data.len());
  assert_eq!(&received_buf[..bytes_received as usize], send_data.as_slice());

  // Cleanup
  let (sender_close, receiver_close) = mpsc::channel();

  lio::close(client_sock).send_with(sender_close.clone());
  lio::close(client_fd).send_with(sender_close.clone());
  lio::close(server_sock).send_with(sender_close.clone());

  // assert_eq!(receiver_close.try_recv().unwrap_err(), TryRecvError::Empty);
  lio::tick();

  receiver_close.try_recv().unwrap().expect("Failed to close client");
  receiver_close.try_recv().unwrap().expect("Failed to close server client");
  receiver_close.try_recv().unwrap().expect("Failed to close server");
}

#[ignore = "flaky network test"]
#[test]
fn test_recv_on_closed() {
  lio::init();

  let (sender_sock, receiver_sock) = mpsc::channel();
  let (sender_unit, receiver_unit) = mpsc::channel();

  // Create server socket
  socket(Domain::IPV4, Type::STREAM, Some(Protocol::TCP))
    .send_with(sender_sock.clone());

  // assert_eq!(receiver_sock.try_recv().unwrap_err(), TryRecvError::Empty);
  lio::tick();

  let server_sock =
    receiver_sock.try_recv().unwrap().expect("Failed to create server socket");

  let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();

  bind(server_sock, addr).send_with(sender_unit.clone());

  // assert_eq!(receiver_unit.try_recv().unwrap_err(), TryRecvError::Empty);
  lio::tick();

  receiver_unit.try_recv().unwrap().expect("Failed to bind");

  let bound_addr = unsafe {
    let mut addr_storage = MaybeUninit::<libc::sockaddr_in>::zeroed();
    let mut addr_len =
      std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
    libc::getsockname(
      server_sock,
      addr_storage.as_mut_ptr() as *mut libc::sockaddr,
      &mut addr_len,
    );
    let sockaddr_in = addr_storage.assume_init();
    let port = u16::from_be(sockaddr_in.sin_port);
    format!("127.0.0.1:{}", port).parse::<SocketAddr>().unwrap()
  };

  listen(server_sock, 128).send_with(sender_unit.clone());

  // assert_eq!(receiver_unit.try_recv().unwrap_err(), TryRecvError::Empty);
  lio::tick();

  receiver_unit.try_recv().unwrap().expect("Failed to listen");

  // Create client socket
  socket(Domain::IPV4, Type::STREAM, Some(Protocol::TCP))
    .send_with(sender_sock.clone());

  // assert_eq!(receiver_sock.try_recv().unwrap_err(), TryRecvError::Empty);
  lio::tick();

  let client_sock =
    receiver_sock.try_recv().unwrap().expect("Failed to create client socket");

  // Accept and connect
  let (sender_a, receiver_a) = mpsc::channel();
  let (sender_c, receiver_c) = mpsc::channel();

  accept(server_sock).send_with(sender_a.clone());

  connect(client_sock, bound_addr).send_with(sender_c.clone());

  // assert_eq!(receiver_c.try_recv().unwrap_err(), TryRecvError::Empty);
  lio::tick();
  lio::tick();

  let (server_client_fd, _client_addr) =
    receiver_a.try_recv().unwrap().expect("Failed to accept");
  receiver_c.try_recv().unwrap().expect("Failed to connect");

  // Close client
  unsafe {
    libc::close(client_sock);
  }

  // Try to receive on closed connection
  let buf = vec![0u8; 1024];
  let (sender_recv, receiver_recv) = mpsc::channel();
  recv(server_client_fd, buf, None).send_with(sender_recv.clone());

  // assert_eq!(receiver_recv.try_recv().unwrap_err(), TryRecvError::Empty);
  lio::tick();

  let (bytes_received, _) = receiver_recv.try_recv().unwrap();
  let bytes_received =
    bytes_received.expect("recv should succeed but return 0");

  // Should return 0 when connection is closed
  assert_eq!(bytes_received, 0);

  // Cleanup
  let (sender_close, receiver_close) = mpsc::channel();

  let sender_close1 = sender_close.clone();
  lio::close(server_client_fd).when_done(move |res| {
    sender_close1.send(res).unwrap();
  });

  let sender_close2 = sender_close.clone();
  lio::close(server_sock).when_done(move |res| {
    sender_close2.send(res).unwrap();
  });

  // assert_eq!(receiver_close.try_recv().unwrap_err(), TryRecvError::Empty);
  lio::tick();

  receiver_close.try_recv().unwrap().expect("Failed to close server client");
  receiver_close.try_recv().unwrap().expect("Failed to close server");
}

#[ignore = "flaky network test"]
#[test]
fn prop_test_recv_arbitrary_data() {
  lio::init();

  let mut runner = TestRunner::new(Config::default());

  runner
    .run(&(1usize..=8192, any::<u64>()), |thing| {
      let (data_size, seed) = thing;
      prop_test_recv_arbitrary_data_run(data_size, seed)
    })
    .unwrap();
}

fn prop_test_recv_arbitrary_data_run(
  data_size: usize,
  seed: u64,
) -> Result<(), TestCaseError> {
  // Generate deterministic random data
  let test_data: Vec<u8> = (0..data_size)
    .map(|i| ((seed.wrapping_add(i as u64)) % 256) as u8)
    .collect();

  let mut socket_recv =
    socket(Domain::IPV4, Type::STREAM, Some(Protocol::TCP)).send();

  // assert_eq!(receiver_sock.try_recv().unwrap_err(), TryRecvError::Empty);
  lio::tick();

  let server_sock =
    socket_recv.try_recv().unwrap().expect("Failed to create server socket");

  // Bind
  let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
  let mut bind_recv = bind(server_sock, addr).send();

  lio::tick();

  bind_recv
    .try_recv()
    .expect("It should return directly")
    .expect("'bind' failed");

  // Get bound address
  let bound_addr = unsafe {
    let mut addr_storage = MaybeUninit::<libc::sockaddr_in>::zeroed();
    let mut addr_len =
      std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
    libc::getsockname(
      server_sock,
      addr_storage.as_mut_ptr() as *mut libc::sockaddr,
      &mut addr_len,
    );
    let sockaddr_in = addr_storage.assume_init();
    let port = u16::from_be(sockaddr_in.sin_port);
    format!("127.0.0.1:{}", port).parse::<SocketAddr>().unwrap()
  };

  let mut listen_recv = listen(server_sock, 128).send();

  // assert_eq!(receiver_unit.try_recv().unwrap_err(), TryRecvError::Empty);
  lio::tick();

  listen_recv.try_recv().expect("listen done").unwrap();

  // Create client socket
  let mut client_socket =
    socket(Domain::IPV4, Type::STREAM, Some(Protocol::TCP)).send();

  lio::tick();

  let client_sock =
    client_socket.try_recv().unwrap().expect("Failed to create client socket");

  let mut accept_sock = accept(server_sock).send();

  let mut connect_sock = connect(client_sock, bound_addr).send();

  // assert_eq!(receiver_a.try_recv().unwrap_err(), TryRecvError::Empty);
  lio::tick();
  lio::tick();

  let server_client_fd =
    accept_sock.try_recv().unwrap().expect("Accept failed").0;
  connect_sock.try_recv().unwrap().expect("Connect failed");

  // Send data from client using libc
  let sent = unsafe {
    libc::send(
      client_sock,
      test_data.as_ptr() as *const libc::c_void,
      test_data.len(),
      0,
    )
  };
  assert_eq!(sent as usize, test_data.len(), "Send failed");

  // Test recv on server side
  let recv_buf = vec![0u8; data_size];
  let (sender_recv, receiver_recv) = mpsc::channel();
  recv(server_client_fd, recv_buf, None).when_done(move |res| {
    sender_recv.send(res).unwrap();
  });

  // assert_eq!(receiver_recv.try_recv().unwrap_err(), TryRecvError::Empty);
  lio::tick();

  let (recv_result, received_buf) = receiver_recv.recv().unwrap();
  let bytes_received = recv_result.expect("Recv failed") as usize;

  // Verify
  assert!(bytes_received > 0, "Should receive at least some bytes");
  assert_eq!(
    &received_buf[..bytes_received],
    &test_data[..bytes_received],
    "Received data should match sent data"
  );

  // Cleanup
  let (sender_close, receiver_close) = mpsc::channel();

  lio::close(client_sock).send_with(sender_close.clone());
  lio::close(server_client_fd).send_with(sender_close.clone());
  lio::close(server_sock).send_with(sender_close.clone());

  // assert_eq!(receiver_close.try_recv().unwrap_err(), TryRecvError::Empty);
  lio::tick();

  receiver_close.recv().unwrap().expect("Failed to close client");
  receiver_close.recv().unwrap().expect("Failed to close server client");
  receiver_close.recv().unwrap().expect("Failed to close server");

  Ok(())
}
