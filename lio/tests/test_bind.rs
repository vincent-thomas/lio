use lio::bind;
use std::{
  net::SocketAddr,
  os::fd::{AsFd, AsRawFd},
  sync::mpsc::{self},
};

#[test]
fn test_bind_ipv4_any_port() {
  lio::init();

  let (sender_sock, receiver_sock) = mpsc::channel();
  let (sender_bind, receiver_bind) = mpsc::channel();

  let sender_s = sender_sock.clone();
  lio::test_utils::tcp_socket().send_with(sender_s.clone());

  lio::tick();

  let sock = receiver_sock.recv().unwrap().expect("Failed to create socket");

  // Bind to 0.0.0.0:0 (any available port)
  let addr: SocketAddr = "0.0.0.0:0".parse().unwrap();

  let sender_b = sender_bind.clone();
  bind(sock.clone(), addr).send_with(sender_b.clone());
  // assert_eq!(receiver_bind.try_recv().unwrap_err(), TryRecvError::Empty);

  lio::tick();

  receiver_bind.recv().unwrap().expect("Failed to bind socket");

  // Verify binding by getting the socket name
  unsafe {
    let mut addr_storage =
      std::mem::MaybeUninit::<libc::sockaddr_storage>::zeroed();
    let mut addr_len =
      std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    let result = libc::getsockname(
      sock.clone().as_fd().as_raw_fd(),
      addr_storage.as_mut_ptr() as *mut libc::sockaddr,
      &mut addr_len,
    );
    assert_eq!(result, 0, "getsockname should succeed");
    libc::close(sock.as_fd().as_raw_fd());
  }

  lio::exit();
}

// #[ignore = "flaky network test"]
#[test]
fn test_bind_ipv4_specific_port() {
  lio::init();

  let (sender_sock, receiver_sock) = mpsc::channel();
  let (sender_bind, receiver_bind) = mpsc::channel();

  let sender_s = sender_sock.clone();
  lio::test_utils::tcp_socket().send_with(sender_s.clone());

  lio::tick();

  let sock = receiver_sock.recv().unwrap().expect("Failed to create socket");

  // Bind to a high port number
  let addr: SocketAddr = "127.0.0.1:19999".parse().unwrap();

  let sender_b = sender_bind.clone();
  bind(sock.clone(), addr).when_done(move |res| {
    sender_b.send(res).unwrap();
  });

  lio::tick();

  receiver_bind.recv().unwrap().expect("Failed to bind to specific port");

  // Verify the port
  unsafe {
    let mut addr_storage = std::mem::MaybeUninit::<libc::sockaddr_in>::zeroed();
    let mut addr_len =
      std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
    libc::getsockname(
      sock.as_fd().as_raw_fd(),
      addr_storage.as_mut_ptr() as *mut libc::sockaddr,
      &mut addr_len,
    );
    let sockaddr_in = addr_storage.assume_init();
    assert_eq!(u16::from_be(sockaddr_in.sin_port), 19999);
    libc::close(sock.as_fd().as_raw_fd());
  }
  lio::exit();
}

#[test]
fn test_bind_ipv6() {
  println!("[test_bind_ipv6] Starting test");
  lio::init();
  println!("[test_bind_ipv6] lio::init() completed");

  let (sender_sock, receiver_sock) = mpsc::channel();
  let (sender_bind, receiver_bind) = mpsc::channel();

  let sender_s = sender_sock.clone();
  println!("[test_bind_ipv6] Calling tcp6_socket().send_with()");
  lio::test_utils::tcp6_socket().when_done(move |res| {
    println!(
      "[test_bind_ipv6] tcp6_socket callback invoked with result: {:?}",
      res
    );
    sender_s.send(res).unwrap();
  });

  println!("[test_bind_ipv6] Calling first lio::tick() (should be no-op)");
  lio::tick();
  println!("[test_bind_ipv6] First lio::tick() completed");

  println!(
    "[test_bind_ipv6] Waiting to receive socket from channel (blocking recv with timeout)"
  );
  let sock = match receiver_sock.recv_timeout(std::time::Duration::from_secs(2))
  {
    Ok(res) => {
      println!("[test_bind_ipv6] Socket operation completed");
      res.expect("Failed to create IPv6 socket")
    }
    Err(e) => {
      println!("[test_bind_ipv6] ERROR: Timeout waiting for socket: {:?}", e);
      panic!("Socket creation timed out after 2 seconds");
    }
  };
  println!("[test_bind_ipv6] Socket received: fd={}", sock.as_fd().as_raw_fd());

  // Bind to IPv6 any address
  let addr: SocketAddr = "[::]:0".parse().unwrap();
  println!("[test_bind_ipv6] Binding to address: {}", addr);

  let sender_b = sender_bind.clone();
  bind(sock.clone(), addr).send_with(sender_b.clone());

  println!("[test_bind_ipv6] Calling second lio::tick() (should be no-op)");
  lio::tick();
  println!("[test_bind_ipv6] Second lio::tick() completed");

  println!(
    "[test_bind_ipv6] Waiting to receive bind result from channel (blocking recv with timeout)"
  );
  match receiver_bind.recv_timeout(std::time::Duration::from_secs(2)) {
    Ok(res) => {
      println!("[test_bind_ipv6] Bind operation completed");
      res.expect("Failed to bind IPv6 socket");
      println!("[test_bind_ipv6] Bind result received successfully");
    }
    Err(e) => {
      println!(
        "[test_bind_ipv6] ERROR: Timeout waiting for bind result: {:?}",
        e
      );
      panic!("Bind operation timed out after 2 seconds");
    }
  }

  // Verify binding
  unsafe {
    let mut addr_storage =
      std::mem::MaybeUninit::<libc::sockaddr_storage>::zeroed();
    let mut addr_len =
      std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    let result = libc::getsockname(
      sock.as_fd().as_raw_fd(),
      addr_storage.as_mut_ptr() as *mut libc::sockaddr,
      &mut addr_len,
    );
    assert_eq!(result, 0);
    println!("[test_bind_ipv6] Verification completed, closing socket");
    libc::close(sock.as_fd().as_raw_fd());
  }
  println!("[test_bind_ipv6] Test completed successfully");
  lio::exit();
}
//
// #[ignore = "flaky network test"]
// #[test]
// fn test_bind_udp() {
//   lio::init();
//
//   let (sender_sock, receiver_sock) = mpsc::channel();
//   let (sender_bind, receiver_bind) = mpsc::channel();
//
//   let sender_s = sender_sock.clone();
//   lio::test_utils::udp_socket().when_done(move |res| {
//     sender_s.send(res).unwrap();
//   });
//
//   // assert_eq!(receiver_sock.try_recv().unwrap_err(), TryRecvError::Empty);
//
//   lio::tick();
//
//   let sock =
//     receiver_sock.recv().unwrap().expect("Failed to create UDP socket");
//
//   let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
//
//   let sender_b = sender_bind.clone();
//   bind(sock, addr).when_done(move |res| {
//     sender_b.send(res).unwrap();
//   });
//
//   // assert_eq!(receiver_bind.try_recv().unwrap_err(), TryRecvError::Empty);
//
//   lio::tick();
//
//   receiver_bind.recv().unwrap().expect("Failed to bind UDP socket");
//
//   // Verify binding
//   unsafe {
//     let mut addr_storage =
//       std::mem::MaybeUninit::<libc::sockaddr_storage>::zeroed();
//     let mut addr_len =
//       std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
//     let result = libc::getsockname(
//       sock,
//       addr_storage.as_mut_ptr() as *mut libc::sockaddr,
//       &mut addr_len,
//     );
//     assert_eq!(result, 0);
//     libc::close(sock);
//   }
// }
//
// #[ignore = "flaky network test"]
// #[test]
// fn test_bind_already_bound() {
//   lio::init();
//
//   let (sender_sock, receiver_sock) = mpsc::channel();
//   let (sender_bind, receiver_bind) = mpsc::channel();
//
//   let sender_s1 = sender_sock.clone();
//   lio::test_utils::tcp_socket().when_done(move |res| {
//     sender_s1.send(res).unwrap();
//   });
//
//   // assert_eq!(receiver_sock.try_recv().unwrap_err(), TryRecvError::Empty);
//
//   lio::tick();
//
//   let sock1 =
//     receiver_sock.recv().unwrap().expect("Failed to create first socket");
//
//   let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
//
//   let sender_b1 = sender_bind.clone();
//   bind(sock1, addr).when_done(move |res| {
//     sender_b1.send(res).unwrap();
//   });
//
//   // assert_eq!(receiver_bind.try_recv().unwrap_err(), TryRecvError::Empty);
//
//   lio::tick();
//
//   receiver_bind.recv().unwrap().expect("Failed to bind first socket");
//
//   // Get the actual bound address
//   let bound_addr = unsafe {
//     let mut addr_storage = std::mem::MaybeUninit::<libc::sockaddr_in>::zeroed();
//     let mut addr_len =
//       std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
//     libc::getsockname(
//       sock1,
//       addr_storage.as_mut_ptr() as *mut libc::sockaddr,
//       &mut addr_len,
//     );
//     addr_storage.assume_init()
//   };
//
//   // Try to bind another socket to the same address
//   let sender_s2 = sender_sock.clone();
//   lio::test_utils::tcp_socket().when_done(move |res| {
//     sender_s2.send(res).unwrap();
//   });
//
//   // assert_eq!(receiver_sock.try_recv().unwrap_err(), TryRecvError::Empty);
//
//   lio::tick();
//
//   let sock2 =
//     receiver_sock.recv().unwrap().expect("Failed to create second socket");
//
//   let port = u16::from_be(bound_addr.sin_port);
//   let addr2: SocketAddr = format!("127.0.0.1:{}", port).parse().unwrap();
//
//   let (sender_bind2, receiver_bind2) = mpsc::channel();
//   let sender_b2 = sender_bind2.clone();
//   bind(sock2, addr2).when_done(move |res| {
//     sender_b2.send(res).unwrap();
//   });
//
//   // assert_eq!(receiver_bind2.try_recv().unwrap_err(), TryRecvError::Empty);
//
//   lio::tick();
//
//   let result = receiver_bind2.recv().unwrap();
//
//   // Should fail with address in use
//   assert!(result.is_err(), "Binding to already-used address should fail");
//
//   // Cleanup
//   unsafe {
//     libc::close(sock1);
//     libc::close(sock2);
//   }
// }
//
// #[ignore = "flaky network test"]
// #[test]
// fn test_bind_double_bind() {
//   lio::init();
//
//   let (sender_sock, receiver_sock) = mpsc::channel();
//   let (sender_bind, receiver_bind) = mpsc::channel();
//
//   let sender_s = sender_sock.clone();
//   lio::test_utils::tcp_socket().when_done(move |res| {
//     sender_s.send(res).unwrap();
//   });
//
//   // assert_eq!(receiver_sock.try_recv().unwrap_err(), TryRecvError::Empty);
//
//   lio::tick();
//
//   let sock = receiver_sock.recv().unwrap().expect("Failed to create socket");
//
//   let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
//
//   let sender_b1 = sender_bind.clone();
//   bind(sock, addr).when_done(move |res| {
//     sender_b1.send(res).unwrap();
//   });
//
//   // assert_eq!(receiver_bind.try_recv().unwrap_err(), TryRecvError::Empty);
//
//   lio::tick();
//
//   receiver_bind.recv().unwrap().expect("First bind should succeed");
//
//   // Try to bind the same socket again
//   let (sender_bind2, receiver_bind2) = mpsc::channel();
//   let sender_b2 = sender_bind2.clone();
//   bind(sock, addr).when_done(move |res| {
//     sender_b2.send(res).unwrap();
//   });
//
//   // assert_eq!(receiver_bind2.try_recv().unwrap_err(), TryRecvError::Empty);
//
//   lio::tick();
//
//   let result = receiver_bind2.recv().unwrap();
//
//   // Should fail
//   assert!(result.is_err(), "Double bind should fail");
//
//   // Cleanup
//   unsafe {
//     libc::close(sock);
//   }
// }
//
// #[ignore = "flaky network test"]
// #[test]
// fn test_bind_with_reuseaddr() {
//   lio::init();
//
//   let (sender_sock, receiver_sock) = mpsc::channel();
//   let (sender_bind, receiver_bind) = mpsc::channel();
//
//   let sender_s1 = sender_sock.clone();
//   lio::test_utils::tcp_socket().when_done(move |res| {
//     sender_s1.send(res).unwrap();
//   });
//
//   // assert_eq!(receiver_sock.try_recv().unwrap_err(), TryRecvError::Empty);
//
//   lio::tick();
//
//   let sock1 =
//     receiver_sock.recv().unwrap().expect("Failed to create first socket");
//
//   // Enable SO_REUSEADDR on first socket
//   unsafe {
//     let reuse: i32 = 1;
//     libc::setsockopt(
//       sock1,
//       libc::SOL_SOCKET,
//       libc::SO_REUSEADDR,
//       &reuse as *const _ as *const libc::c_void,
//       std::mem::size_of::<i32>() as libc::socklen_t,
//     );
//   }
//
//   let addr: SocketAddr = "127.0.0.1:18888".parse().unwrap();
//
//   let sender_b1 = sender_bind.clone();
//   bind(sock1, addr).when_done(move |res| {
//     sender_b1.send(res).unwrap();
//   });
//
//   // assert_eq!(receiver_bind.try_recv().unwrap_err(), TryRecvError::Empty);
//
//   lio::tick();
//
//   receiver_bind.recv().unwrap().expect("Failed to bind first socket");
//
//   // Close first socket
//   unsafe {
//     libc::close(sock1);
//   }
//
//   // Immediately bind another socket to the same address with SO_REUSEADDR
//   let sender_s2 = sender_sock.clone();
//   lio::test_utils::tcp_socket().when_done(move |res| {
//     sender_s2.send(res).unwrap();
//   });
//
//   // assert_eq!(receiver_sock.try_recv().unwrap_err(), TryRecvError::Empty);
//
//   lio::tick();
//
//   let sock2 =
//     receiver_sock.recv().unwrap().expect("Failed to create second socket");
//
//   unsafe {
//     let reuse: i32 = 1;
//     libc::setsockopt(
//       sock2,
//       libc::SOL_SOCKET,
//       libc::SO_REUSEADDR,
//       &reuse as *const _ as *const libc::c_void,
//       std::mem::size_of::<i32>() as libc::socklen_t,
//     );
//   }
//
//   let (sender_bind2, receiver_bind2) = mpsc::channel();
//   let sender_b2 = sender_bind2.clone();
//   bind(sock2, addr).when_done(move |res| {
//     sender_b2.send(res).unwrap();
//   });
//
//   // assert_eq!(receiver_bind2.try_recv().unwrap_err(), TryRecvError::Empty);
//
//   lio::tick();
//
//   receiver_bind2.recv().unwrap().expect(
//     "Should be able to bind with SO_REUSEADDR after closing previous socket",
//   );
//
//   // Cleanup
//   unsafe {
//     libc::close(sock2);
//   }
// }
//
// #[ignore = "flaky network test"]
// #[test]
// fn test_bind_localhost() {
//   lio::init();
//
//   let (sender_sock, receiver_sock) = mpsc::channel();
//   let (sender_bind, receiver_bind) = mpsc::channel();
//
//   let sender_s = sender_sock.clone();
//   lio::test_utils::tcp_socket().when_done(move |res| {
//     sender_s.send(res).unwrap();
//   });
//
//   // assert_eq!(receiver_sock.try_recv().unwrap_err(), TryRecvError::Empty);
//
//   lio::tick();
//
//   let sock = receiver_sock.recv().unwrap().expect("Failed to create socket");
//
//   let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
//
//   let sender_b = sender_bind.clone();
//   bind(sock, addr).when_done(move |res| {
//     sender_b.send(res).unwrap();
//   });
//
//   // assert_eq!(receiver_bind.try_recv().unwrap_err(), TryRecvError::Empty);
//
//   lio::tick();
//
//   receiver_bind.recv().unwrap().expect("Failed to bind to localhost");
//
//   // Verify it's bound to localhost
//   unsafe {
//     let mut addr_storage = std::mem::MaybeUninit::<libc::sockaddr_in>::zeroed();
//     let mut addr_len =
//       std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
//     libc::getsockname(
//       sock,
//       addr_storage.as_mut_ptr() as *mut libc::sockaddr,
//       &mut addr_len,
//     );
//     let sockaddr_in = addr_storage.assume_init();
//     // 127.0.0.1 in network byte order
//     assert_eq!(u32::from_be(sockaddr_in.sin_addr.s_addr), 0x7f000001);
//     libc::close(sock);
//   }
// }
//
// #[ignore = "flaky network test"]
// #[test]
// fn test_bind_concurrent() {
//   lio::init();
//
//   let (sender_sock, receiver_sock) = mpsc::channel();
//   let (sender_bind, receiver_bind) = mpsc::channel();
//
//   // Test binding multiple sockets concurrently to different ports
//   for port in 20000..20010 {
//     let sender_s = sender_sock.clone();
//     lio::test_utils::tcp_socket().when_done(move |res| {
//       sender_s.send(res).unwrap();
//     });
//   }
//
//   // assert_eq!(receiver_sock.try_recv().unwrap_err(), TryRecvError::Empty);
//
//   lio::tick();
//
//   let mut socks = Vec::new();
//   for _ in 0..10 {
//     let sock = receiver_sock.recv().unwrap().expect("Failed to create socket");
//     socks.push(sock);
//   }
//
//   // Set SO_REUSEADDR and bind each socket
//   for (i, &sock) in socks.iter().enumerate() {
//     // unsafe {
//     //   let reuse: i32 = 1;
//     //   libc::setsockopt(
//     //     sock,
//     //     libc::SOL_SOCKET,
//     //     libc::SO_REUSEADDR,
//     //     &reuse as *const _ as *const libc::c_void,
//     //     std::mem::size_of::<i32>() as libc::socklen_t,
//     //   );
//     // }
//     //
//     let port = 20000 + i;
//     let addr: SocketAddr = format!("127.0.0.1:{}", port).parse().unwrap();
//     let sender_b = sender_bind.clone();
//     bind(sock, addr).when_done(move |res| {
//       sender_b.send(res).unwrap();
//     });
//   }
//
//   // assert_eq!(receiver_bind.try_recv().unwrap_err(), TryRecvError::Empty);
//
//   lio::tick();
//
//   for _ in 0..10 {
//     receiver_bind.recv().unwrap().expect("Failed to bind socket");
//   }
//
//   // Cleanup
//   for sock in socks {
//     unsafe {
//       libc::close(sock);
//     }
//   }
// }
