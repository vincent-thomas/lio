#![allow(
  clippy::duplicate_mod,
  clippy::unnecessary_mut_passed,
  clippy::expect_fun_call
)]

use super::common::{self, poll_until_recv};
use lio::Lio;
use lio::api::bind;
use std::{net::SocketAddr, sync::mpsc};

#[test]
fn test_bind_ipv4_any_port() {
  let mut lio = Lio::new(64).unwrap();

  let (sender_bind, receiver_bind) = mpsc::channel();
  let sock = common::tcp_socket();

  // Bind to 0.0.0.0:0 (any available port)
  let addr: SocketAddr = "0.0.0.0:0".parse().unwrap();

  bind(&sock, addr).with_lio(&mut lio).send_with(sender_bind.clone());

  poll_until_recv(&mut lio, &receiver_bind).expect("Failed to bind socket");

  // Verify binding by getting the socket name
  let bound = common::get_bound_addr(&sock);
  assert!(bound.is_ipv4() && bound.port() != 0);
}

#[test]
fn test_bind_ipv4_specific_port() {
  let mut lio = Lio::new(64).unwrap();

  let (sender_bind, receiver_bind) = mpsc::channel();
  let sock = common::tcp_socket();

  // Bind to a high port number
  let addr: SocketAddr = "127.0.0.1:19999".parse().unwrap();

  bind(&sock, addr).with_lio(&mut lio).send_with(sender_bind.clone());

  poll_until_recv(&mut lio, &receiver_bind)
    .expect("Failed to bind to specific port");

  // Verify the port
  assert_eq!(common::get_bound_addr(&sock).port(), 19999);
}

#[test]
fn test_bind_ipv6() {
  let mut lio = Lio::new(64).unwrap();

  let (sender_bind, receiver_bind) = mpsc::channel();
  let sock = common::tcp6_socket();

  // Bind to IPv6 any address
  let addr: SocketAddr = "[::]:0".parse().unwrap();

  bind(&sock, addr).with_lio(&mut lio).send_with(sender_bind.clone());

  poll_until_recv(&mut lio, &receiver_bind)
    .expect("Failed to bind IPv6 socket");

  // Verify binding
  assert!(common::get_bound_addr(&sock).is_ipv6());
}

#[test]
fn test_bind_udp() {
  let mut lio = Lio::new(64).unwrap();

  let (sender_bind, receiver_bind) = mpsc::channel();
  let sock = common::udp_socket();

  let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();

  bind(&sock, addr).with_lio(&mut lio).send_with(sender_bind.clone());

  poll_until_recv(&mut lio, &receiver_bind).expect("Failed to bind UDP socket");

  // Verify binding
  assert_ne!(common::get_bound_addr(&sock).port(), 0);
}

#[test]
#[cfg_attr(
  any(target_os = "freebsd", target_os = "macos"),
  ignore = "FreeBSD/macOS allows duplicate binds"
)]
fn test_bind_already_bound() {
  let mut lio = Lio::new(64).unwrap();

  let (sender_bind, receiver_bind) = mpsc::channel();
  let sock1 = common::tcp_socket();

  let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();

  bind(&sock1, addr).with_lio(&mut lio).send_with(sender_bind.clone());

  poll_until_recv(&mut lio, &receiver_bind)
    .expect("Failed to bind first socket");

  // Put socket in listening state to actually claim the port
  let (sender_listen, receiver_listen) = mpsc::channel();
  lio::api::listen(&sock1, 1).with_lio(&mut lio).send_with(sender_listen);
  poll_until_recv(&mut lio, &receiver_listen).expect("Failed to listen");

  // Try to bind another socket to the same address
  let sock2 = common::tcp_socket();
  let addr2 = common::get_bound_addr(&sock1);

  let (sender_bind2, receiver_bind2) = mpsc::channel();
  bind(&sock2, addr2).with_lio(&mut lio).send_with(sender_bind2.clone());

  let result = poll_until_recv(&mut lio, &receiver_bind2);

  // Should fail with address in use
  assert!(result.is_err(), "Binding to already-used address should fail");
}

#[test]
fn test_bind_double_bind() {
  let mut lio = Lio::new(64).unwrap();

  let (sender_bind, receiver_bind) = mpsc::channel();
  let sock = common::tcp_socket();

  let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();

  bind(&sock, addr).with_lio(&mut lio).send_with(sender_bind.clone());

  poll_until_recv(&mut lio, &receiver_bind).expect("First bind should succeed");

  // Try to bind the same socket again
  let (sender_bind2, receiver_bind2) = mpsc::channel();
  bind(&sock, addr).with_lio(&mut lio).send_with(sender_bind2);

  let result = poll_until_recv(&mut lio, &receiver_bind2);

  // Should fail
  assert!(result.is_err(), "Double bind should fail");
}

#[test]
fn test_bind_with_reuseaddr() {
  let mut lio = Lio::new(64).unwrap();

  let (sender_bind, receiver_bind) = mpsc::channel();
  let sock1 = common::tcp_socket();

  // Enable SO_REUSEADDR on first socket
  common::set_reuse_addr(&sock1);

  let addr: SocketAddr = "127.0.0.1:18888".parse().unwrap();

  bind(&sock1, addr).with_lio(&mut lio).send_with(sender_bind.clone());

  poll_until_recv(&mut lio, &receiver_bind)
    .expect("Failed to bind first socket");

  // Close first socket (drop through Resource to avoid double-close)
  drop(sock1);

  // Immediately bind another socket to the same address with SO_REUSEADDR
  let sock2 = common::tcp_socket();

  common::set_reuse_addr(&sock2);

  let (sender_bind2, receiver_bind2) = mpsc::channel();
  bind(&sock2, addr).with_lio(&mut lio).send_with(sender_bind2.clone());

  poll_until_recv(&mut lio, &receiver_bind2).expect(
    "Should be able to bind with SO_REUSEADDR after closing previous socket",
  );
}

#[test]
fn test_bind_localhost() {
  let mut lio = Lio::new(64).unwrap();

  let (sender_bind, receiver_bind) = mpsc::channel();
  let sock = common::tcp_socket();

  let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();

  bind(&sock, addr).with_lio(&mut lio).send_with(sender_bind.clone());

  poll_until_recv(&mut lio, &receiver_bind)
    .expect("Failed to bind to localhost");

  // Verify it's bound to localhost
  assert!(common::get_bound_addr(&sock).ip().is_loopback());
}

#[test]
fn test_bind_concurrent() {
  let mut lio = Lio::new(64).unwrap();

  let (sender_bind, receiver_bind) = mpsc::channel();

  // Test binding multiple sockets concurrently to different ports
  let socks: Vec<_> = (0..10).map(|_| common::tcp_socket()).collect();

  // Set SO_REUSEADDR and bind each socket
  for sock in socks.iter() {
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    bind(sock, addr).with_lio(&mut lio).send_with(sender_bind.clone());
  }

  for _ in 0..10 {
    poll_until_recv(&mut lio, &receiver_bind).expect("Failed to bind socket");
  }
}
