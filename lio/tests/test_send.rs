// use lio::api;
// use proptest::prelude::*;
// use proptest::test_runner::TestRunner;
// use std::mem::MaybeUninit;
// use std::net::SocketAddr;
// use std::os::fd::AsRawFd;
// use std::sync::mpsc;
//
// #[test]
// fn test_send_basic() {
//   lio::init();
//
//   // Setup server
//   let server_sock = lio::test_utils::tcp_socket().send();
//
//   lio::tick();
//
//   let server_sock = server_sock.recv().expect("Failed to create server socket");
//
//   let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
//   let bind_recv = api::bind(&server_sock, addr).send();
//
//   lio::tick();
//
//   bind_recv.recv().unwrap();
//
//   let bound_addr = unsafe {
//     let mut addr_storage = MaybeUninit::<libc::sockaddr_in>::zeroed();
//     let mut addr_len =
//       std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
//     libc::getsockname(
//       server_sock.as_raw_fd(),
//       addr_storage.as_mut_ptr() as *mut libc::sockaddr,
//       &mut addr_len,
//     );
//     let sockaddr_in = addr_storage.assume_init();
//     let port = u16::from_be(sockaddr_in.sin_port);
//     format!("127.0.0.1:{}", port).parse::<SocketAddr>().unwrap()
//   };
//
//   let listen_recv = api::listen(&server_sock, 128).send();
//
//   lio::tick();
//
//   listen_recv.recv().expect("Failed to listen");
//   println!("after listen");
//
//   // Create client socket
//   let client_sock = lio::test_utils::tcp_socket().send();
//
//   lio::tick();
//
//   let client_sock = client_sock.recv().unwrap();
//
//   // Connect client to server
//   let connect_recv = api::connect(&client_sock, bound_addr).send();
//
//   lio::tick();
//
//   connect_recv.recv().unwrap();
//
//   // Accept connection on server
//   let accept_recv = api::accept(&server_sock).send();
//
//   lio::tick();
//   lio::tick();
//
//   let (server_client_fd, _addr) = accept_recv.recv().unwrap();
//
//   // Send data
//   let data = b"Hello, Server!".to_vec();
//   let send_recv = api::send(&client_sock, data.clone(), None).send();
//
//   lio::tick();
//
//   let (bytes_sent, returned_buf) = send_recv.recv();
//   let bytes_sent = bytes_sent.expect("Failed to send data");
//
//   assert_eq!(bytes_sent as usize, data.len());
//   assert_eq!(returned_buf, data);
//
//   unsafe {
//     libc::close(client_sock.as_raw_fd());
//     libc::close(server_client_fd.as_raw_fd());
//     libc::close(server_sock.as_raw_fd());
//   }
// }
//
// #[test]
// fn test_send_large_data() {
//   lio::init();
//
//   let server_sock = lio::test_utils::tcp_socket().send();
//
//   lio::tick();
//
//   let server_sock = server_sock.recv().expect("Failed to create server socket");
//
//   let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
//   let bind_recv = api::bind(&server_sock, addr).send();
//
//   lio::tick();
//
//   bind_recv.recv().expect("Failed to bind");
//
//   let bound_addr = unsafe {
//     let mut addr_storage = MaybeUninit::<libc::sockaddr_in>::zeroed();
//     let mut addr_len =
//       std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
//     libc::getsockname(
//       server_sock.as_raw_fd(),
//       addr_storage.as_mut_ptr() as *mut libc::sockaddr,
//       &mut addr_len,
//     );
//     let sockaddr_in = addr_storage.assume_init();
//     let port = u16::from_be(sockaddr_in.sin_port);
//     format!("127.0.0.1:{}", port).parse::<SocketAddr>().unwrap()
//   };
//
//   let listen_recv = api::listen(&server_sock, 128).send();
//
//   lio::tick();
//
//   listen_recv.recv().expect("Failed to listen");
//
//   // Create client socket
//   let client_sock = lio::test_utils::tcp_socket().send();
//
//   lio::tick();
//
//   let client_sock = client_sock.recv().unwrap();
//
//   // Connect client to server
//   let connect_recv = api::connect(&client_sock, bound_addr).send();
//
//   lio::tick();
//
//   connect_recv.recv().unwrap();
//
//   // Accept connection on server
//   let accept_recv = api::accept(&server_sock).send();
//
//   lio::tick();
//   lio::tick();
//
//   let (server_client_fd, _addr) = accept_recv.recv().unwrap();
//
//   // Send large data (1MB)
//   let large_data: Vec<u8> = (0..1024 * 1024).map(|i| (i % 256) as u8).collect();
//   let send_recv = api::send(&client_sock, large_data.clone(), None).send();
//
//   lio::tick();
//
//   let (bytes_sent, returned_buf) = send_recv.recv();
//   let bytes_sent = bytes_sent.expect("Failed to send large data");
//
//   assert!(bytes_sent > 0);
//   assert_eq!(returned_buf, large_data);
//
//   unsafe {
//     libc::close(client_sock.as_raw_fd());
//     libc::close(server_client_fd.as_raw_fd());
//     libc::close(server_sock.as_raw_fd());
//   }
// }
//
// #[test]
// fn test_send_multiple() {
//   lio::init();
//
//   let server_sock = lio::test_utils::tcp_socket().send();
//
//   lio::tick();
//
//   let server_sock = server_sock.recv().expect("Failed to create server socket");
//
//   let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
//   let bind_recv = api::bind(&server_sock, addr).send();
//
//   lio::tick();
//
//   bind_recv.recv().expect("Failed to bind");
//
//   let bound_addr = unsafe {
//     let mut addr_storage = MaybeUninit::<libc::sockaddr_in>::zeroed();
//     let mut addr_len =
//       std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
//     libc::getsockname(
//       server_sock.as_raw_fd(),
//       addr_storage.as_mut_ptr() as *mut libc::sockaddr,
//       &mut addr_len,
//     );
//     let sockaddr_in = addr_storage.assume_init();
//     let port = u16::from_be(sockaddr_in.sin_port);
//     format!("127.0.0.1:{}", port).parse::<SocketAddr>().unwrap()
//   };
//
//   let listen_recv = api::listen(&server_sock, 128).send();
//
//   lio::tick();
//
//   listen_recv.recv().expect("Failed to listen");
//
//   // Create client socket
//   let client_sock = lio::test_utils::tcp_socket().send();
//
//   lio::tick();
//
//   let client_sock = client_sock.recv().unwrap();
//
//   // Connect client to server
//   let connect_recv = api::connect(&client_sock, bound_addr).send();
//
//   lio::tick();
//
//   connect_recv.recv().unwrap();
//
//   // Accept connection on server
//   let accept_recv = api::accept(&server_sock).send();
//
//   lio::tick();
//   lio::tick();
//
//   let (server_client_fd, _addr) = accept_recv.recv().unwrap();
//
//   // Send multiple messages
//   for i in 0..5 {
//     let data = format!("Message {}", i).into_bytes();
//     let send_recv = api::send(&client_sock, data.clone(), None).send();
//
//     lio::tick();
//
//     let (bytes_sent, returned_buf) = send_recv.recv();
//     let bytes_sent = bytes_sent.expect("Failed to send");
//     assert_eq!(bytes_sent as usize, data.len());
//     assert_eq!(returned_buf, data);
//   }
//
//   unsafe {
//     libc::close(client_sock.as_raw_fd());
//     libc::close(server_client_fd.as_raw_fd());
//     libc::close(server_sock.as_raw_fd());
//   }
// }
//
// #[test]
// fn test_send_with_flags() {
//   lio::init();
//
//   let server_sock = lio::test_utils::tcp_socket().send();
//
//   lio::tick();
//
//   let server_sock = server_sock.recv().expect("Failed to create server socket");
//
//   let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
//   let mut bind_recv = api::bind(&server_sock, addr).send();
//
//   lio::tick();
//
//   bind_recv.recv().expect("Failed to bind");
//
//   let bound_addr = unsafe {
//     let mut addr_storage = MaybeUninit::<libc::sockaddr_in>::zeroed();
//     let mut addr_len =
//       std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
//     libc::getsockname(
//       server_sock.as_raw_fd(),
//       addr_storage.as_mut_ptr() as *mut libc::sockaddr,
//       &mut addr_len,
//     );
//     let sockaddr_in = addr_storage.assume_init();
//     let port = u16::from_be(sockaddr_in.sin_port);
//     format!("127.0.0.1:{}", port).parse::<SocketAddr>().unwrap()
//   };
//
//   let listen_recv = api::listen(&server_sock, 128).send();
//
//   lio::tick();
//
//   listen_recv.recv().expect("Failed to listen");
//
//   // Create client socket
//   let client_sock = lio::test_utils::tcp_socket().send();
//
//   lio::tick();
//
//   let client_sock = client_sock.recv().unwrap();
//
//   // Connect client to server
//   let connect_recv = api::connect(&client_sock, bound_addr).send();
//
//   lio::tick();
//
//   connect_recv.recv().unwrap();
//
//   // Accept connection on server
//   let accept_recv = api::accept(&server_sock).send();
//
//   lio::tick();
//   lio::tick();
//
//   let (server_client_fd, _addr) = accept_recv.recv().unwrap();
//
//   // Send with flags (0 is a valid flag value)
//   let data = b"Data with flags".to_vec();
//   let send_recv = api::send(&client_sock, data.clone(), Some(0)).send();
//
//   lio::tick();
//
//   let (bytes_sent, returned_buf) = send_recv.recv();
//   let bytes_sent = bytes_sent.expect("Failed to send with flags");
//
//   assert_eq!(bytes_sent as usize, data.len());
//   assert_eq!(returned_buf, data);
//
//   unsafe {
//     libc::close(client_sock.as_raw_fd());
//     libc::close(server_client_fd.as_raw_fd());
//     libc::close(server_sock.as_raw_fd());
//   }
// }
//
// #[test]
// fn test_send_on_closed_socket() {
//   lio::init();
//
//   let server_sock = lio::test_utils::tcp_socket().send();
//
//   lio::tick();
//
//   let server_sock = server_sock.recv().expect("Failed to create server socket");
//
//   let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
//   let bind_recv = api::bind(&server_sock, addr).send();
//
//   lio::tick();
//
//   bind_recv.recv().expect("Failed to bind");
//
//   let bound_addr = unsafe {
//     let mut addr_storage = MaybeUninit::<libc::sockaddr_in>::zeroed();
//     let mut addr_len =
//       std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
//     libc::getsockname(
//       server_sock.as_raw_fd(),
//       addr_storage.as_mut_ptr() as *mut libc::sockaddr,
//       &mut addr_len,
//     );
//     let sockaddr_in = addr_storage.assume_init();
//     let port = u16::from_be(sockaddr_in.sin_port);
//     format!("127.0.0.1:{}", port).parse::<SocketAddr>().unwrap()
//   };
//
//   let listen_recv = api::listen(&server_sock, 128).send();
//
//   lio::tick();
//
//   listen_recv.recv().expect("Failed to listen");
//
//   // Create client socket
//   let client_sock = lio::test_utils::tcp_socket().send();
//
//   lio::tick();
//
//   let client_sock = client_sock.recv().unwrap();
//
//   // Connect client to server
//   let connect_recv = api::connect(&client_sock, bound_addr).send();
//
//   lio::tick();
//
//   connect_recv.recv().unwrap();
//
//   // Accept connection on server
//   let accept_recv = api::accept(&server_sock).send();
//
//   lio::tick();
//   lio::tick();
//
//   let (server_client_fd, _addr) = accept_recv.recv().unwrap();
//
//   // Close server end
//   unsafe {
//     libc::close(server_client_fd.as_raw_fd());
//   }
//
//   // Try to send after server closed
//   let data = b"This should fail".to_vec();
//   let send_recv = api::send(&client_sock, data, None).send();
//
//   lio::tick();
//
//   let (_result, _) = send_recv.recv();
//
//   // May succeed or fail depending on timing, but shouldn't crash
//   unsafe {
//     libc::close(client_sock.as_raw_fd());
//     libc::close(server_sock.as_raw_fd());
//   }
// }
//
// #[test]
// fn test_send_concurrent() {
//   lio::init();
//
//   // Test sending from multiple clients concurrently
//   let server_sock = lio::test_utils::tcp_socket().send();
//
//   lio::tick();
//
//   let server_sock = server_sock.recv().expect("Failed to create server socket");
//
//   let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
//   let bind_recv = api::bind(&server_sock, addr).send();
//
//   lio::tick();
//
//   bind_recv.recv().expect("Failed to bind");
//
//   let bound_addr = unsafe {
//     let mut addr_storage = MaybeUninit::<libc::sockaddr_in>::zeroed();
//     let mut addr_len =
//       std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
//     libc::getsockname(
//       server_sock.as_raw_fd(),
//       addr_storage.as_mut_ptr() as *mut libc::sockaddr,
//       &mut addr_len,
//     );
//     let sockaddr_in = addr_storage.assume_init();
//     let port = u16::from_be(sockaddr_in.sin_port);
//     format!("127.0.0.1:{}", port).parse::<SocketAddr>().unwrap()
//   };
//
//   let mut listen_recv = api::listen(&server_sock, 128).send();
//
//   lio::tick();
//
//   listen_recv.try_recv().unwrap().expect("Failed to listen");
//
//   // Note: Simplified concurrent test without actual concurrency
//   for i in 0..5 {
//     // Create and connect client
//     let mut client_sock = lio::test_utils::tcp_socket().send();
//
//     lio::tick();
//
//     let client_sock = client_sock.try_recv().unwrap().unwrap();
//
//     let mut connect_recv = api::connect(&client_sock, bound_addr).send();
//
//     lio::tick();
//
//     connect_recv.try_recv().unwrap().unwrap();
//
//     // Accept connection
//     let mut accept_recv = api::accept(&server_sock).send();
//
//     lio::tick();
//     lio::tick();
//
//     let (_server_client_fd, _addr) = accept_recv.try_recv().unwrap().unwrap();
//
//     // Send data
//     let data = format!("Client {}", i).into_bytes();
//     let send_recv = api::send(&client_sock, data.clone(), None).send();
//
//     lio::tick();
//
//     let (bytes_sent, _) = send_recv.recv();
//     let bytes_sent = bytes_sent.expect("Failed to send");
//
//     assert_eq!(bytes_sent as usize, data.len());
//
//     let (sender, receiver) = mpsc::channel();
//     // api::close(client_sock.into_unique().unwrap()).send_with(sender.clone());
//     // api::close(_server_client_fd.into_unique().unwrap())
//     //   .send_with(sender.clone());
//
//     lio::tick();
//
//     receiver.try_recv().unwrap().unwrap();
//     receiver.try_recv().unwrap().unwrap();
//   }
//
//   let mut close_recv = api::close(server_sock.into_unique().unwrap()).send();
//
//   lio::tick();
//
//   close_recv.try_recv().unwrap().unwrap();
// }
//
// #[test]
// #[ignore]
// fn prop_test_send_arbitrary_data() {
//   lio::init();
//   let mut runner = TestRunner::new(ProptestConfig::default());
//
//   runner
//     .run(&(1usize..=8192, any::<u64>()), |prop| {
//       prop_test_send_arbitrary_data_run(prop.0, prop.1)
//     })
//     .unwrap();
// }
//
// fn prop_test_send_arbitrary_data_run(
//   data_size: usize,
//   seed: u64,
// ) -> Result<(), TestCaseError> {
//   // Generate deterministic random data based on seed
//   let test_data: Vec<u8> = (0..data_size)
//     .map(|i| ((seed.wrapping_add(i as u64)) % 256) as u8)
//     .collect();
//
//   // Create server socket
//   let server_sock = lio::test_utils::tcp_socket().send();
//
//   lio::tick();
//
//   let server_sock = server_sock.recv().expect("Failed to create server socket");
//
//   // Bind to any available port on localhost
//   let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
//   let bind_recv = api::bind(&server_sock, addr).send();
//
//   lio::tick();
//
//   bind_recv.recv().expect("Failed to bind");
//
//   // Get the actual bound address
//   let bound_addr = unsafe {
//     let mut addr_storage = MaybeUninit::<libc::sockaddr_in>::zeroed();
//     let mut addr_len =
//       std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
//     libc::getsockname(
//       server_sock.as_raw_fd(),
//       addr_storage.as_mut_ptr() as *mut libc::sockaddr,
//       &mut addr_len,
//     );
//     let sockaddr_in = addr_storage.assume_init();
//     let port = u16::from_be(sockaddr_in.sin_port);
//     format!("127.0.0.1:{}", port).parse::<SocketAddr>().unwrap()
//   };
//
//   // Start listening
//   let mut listen_recv = api::listen(&server_sock, 128).send();
//
//   lio::tick();
//   listen_recv.recv().expect("Failed to listen");
//
//   let mut client_sock = lio::test_utils::tcp_socket().send();
//
//   lio::tick();
//
//   let client_sock = client_sock.try_recv().unwrap().unwrap();
//
//   let mut connect_recv = api::connect(&client_sock, bound_addr).send();
//
//   lio::tick();
//
//   let mut accept_recv = api::accept(&server_sock).send();
//
//   lio::tick();
//   lio::tick();
//
//   connect_recv.try_recv().unwrap().unwrap();
//   let (server_client_fd, _) = accept_recv.try_recv().unwrap().unwrap();
//
//   let mut send_recv = api::send(&client_sock, test_data.clone(), None).send();
//   let recv_buf = vec![0u8; data_size];
//   let mut recv_recv = api::recv(&server_client_fd, recv_buf, None).send();
//
//   lio::tick();
//   lio::tick();
//
//   // Send data from client to server
//   let (bytes_sent, returned_buf) = send_recv.try_recv().unwrap();
//   let bytes_sent = bytes_sent.expect("Send failed");
//
//   assert_eq!(
//     bytes_sent as usize,
//     test_data.len(),
//     "Send should return correct byte count"
//   );
//
//   assert_eq!(
//     returned_buf, test_data,
//     "Send returned buffer should match original data"
//   );
//
//   // Receive the data on server side to verify it was sent
//   // FIXME: This sometimes deadlocks.
//   let (bytes_received, received_buf) = recv_recv.recv();
//   let bytes_received = bytes_received.unwrap() as usize;
//
//   assert!(bytes_received > 0, "Should receive at least some bytes");
//   assert_eq!(
//     &received_buf[..bytes_received],
//     &test_data[..bytes_received],
//     "Received data should match sent data"
//   );
//
//   // Cleanup
//
//   let (sender, receiver) = mpsc::channel();
//
//   api::close(client_sock.into_unique().unwrap()).send_with(sender.clone());
//   api::close(server_client_fd.into_unique().unwrap()).send_with(sender.clone());
//   api::close(server_sock.into_unique().unwrap()).send_with(sender.clone());
//
//   lio::tick();
//
//   receiver.try_recv().unwrap().unwrap();
//   receiver.try_recv().unwrap().unwrap();
//   receiver.try_recv().unwrap().unwrap();
//
//   Ok(())
// }
