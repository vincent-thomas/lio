#![allow(
  clippy::duplicate_mod,
  clippy::unnecessary_mut_passed,
  clippy::expect_fun_call
)]

use super::common::{self, as_tcp_stream, poll_until_recv};
use lio::api::resource::Resource;
use lio::{Lio, api};
use std::net::TcpListener;
use std::sync::mpsc;

fn nonblocking(sock: Resource) -> Resource {
  as_tcp_stream(&sock).set_nonblocking(true).expect("failed to set O_NONBLOCK");
  sock
}

fn tcp_socket_v4() -> Resource {
  nonblocking(common::tcp_socket())
}

fn tcp_socket_v6() -> Resource {
  nonblocking(common::tcp6_socket())
}

fn setup_listener_v4() -> (TcpListener, std::net::SocketAddr) {
  let listener =
    TcpListener::bind("127.0.0.1:0").expect("Failed to bind TCP listener");
  let addr = listener.local_addr().expect("Failed to query listener address");
  (listener, addr)
}

fn setup_listener_v6() -> (TcpListener, std::net::SocketAddr) {
  let listener =
    TcpListener::bind("[::1]:0").expect("Failed to bind IPv6 listener");
  let addr =
    listener.local_addr().expect("Failed to query IPv6 listener address");
  (listener, addr)
}

#[test]
fn basic() {
  let mut lio = Lio::new(64).unwrap();
  let (listener, addr) = setup_listener_v4();
  let client_sock = tcp_socket_v4();

  let (sender, receiver) = mpsc::channel();
  api::connect(&client_sock, addr).with_lio(&mut lio).send_with(sender);
  poll_until_recv(&mut lio, &receiver).expect("Failed to connect");

  let (_accepted, _) =
    listener.accept().expect("Failed to accept connected client");

  as_tcp_stream(&client_sock)
    .peer_addr()
    .expect("Should be able to get peer name after connect");
}

#[test]
fn ipv6() {
  let mut lio = Lio::new(64).unwrap();
  let (listener, addr) = setup_listener_v6();
  let client_sock = tcp_socket_v6();

  let (sender, receiver) = mpsc::channel();
  api::connect(&client_sock, addr).with_lio(&mut lio).send_with(sender);
  poll_until_recv(&mut lio, &receiver).expect("Failed to connect IPv6");

  let (_accepted, _) = listener.accept().expect("Failed to accept IPv6 client");

  assert_eq!(as_tcp_stream(&client_sock).peer_addr().unwrap(), addr);
}

#[test]
fn to_nonexistent() {
  let mut lio = Lio::new(64).unwrap();
  let client_sock = tcp_socket_v4();
  let addr = "127.0.0.1:1".parse().unwrap();

  let (sender, receiver) = mpsc::channel();
  api::connect(&client_sock, addr).with_lio(&mut lio).send_with(sender);
  let result = poll_until_recv(&mut lio, &receiver);
  assert!(result.is_err(), "Connect to non-listening port should fail");
}

#[test]
fn multiple_clients() {
  let mut lio = Lio::new(64).unwrap();
  let (listener, addr) = setup_listener_v4();
  let mut clients = Vec::new();

  for _ in 0..5 {
    let client_sock = tcp_socket_v4();
    let (sender, receiver) = mpsc::channel();
    api::connect(&client_sock, addr).with_lio(&mut lio).send_with(sender);
    poll_until_recv(&mut lio, &receiver).expect("Failed to connect");
    clients.push(client_sock);
  }

  for _ in 0..5 {
    let (_accepted, _) = listener.accept().expect("Failed to accept client");
  }

  assert_eq!(clients.len(), 5);
}

#[test]
fn already_connected() {
  let mut lio = Lio::new(64).unwrap();
  let (listener, addr) = setup_listener_v4();
  let client_sock = tcp_socket_v4();

  let (sender, receiver) = mpsc::channel();
  api::connect(&client_sock, addr).with_lio(&mut lio).send_with(sender);
  poll_until_recv(&mut lio, &receiver).expect("First connect should succeed");

  let (_accepted, _) =
    listener.accept().expect("Failed to accept connected client");

  let (sender2, receiver2) = mpsc::channel();
  api::connect(&client_sock, addr).with_lio(&mut lio).send_with(sender2);
  let result = poll_until_recv(&mut lio, &receiver2);
  assert!(result.is_err(), "Second connect should fail: err {result:#?}");
}

#[test]
fn concurrent() {
  let mut lio = Lio::new(64).unwrap();
  let (listener, addr) = setup_listener_v4();
  let (sender, receiver) = mpsc::channel();
  let mut clients = Vec::new();

  for _ in 0..10 {
    let client_sock = tcp_socket_v4();
    api::connect(&client_sock, addr)
      .with_lio(&mut lio)
      .send_with(sender.clone());
    clients.push(client_sock);
  }

  for _ in 0..10 {
    poll_until_recv(&mut lio, &receiver).expect("Failed to connect");
  }

  for _ in 0..10 {
    let (_accepted, _) = listener.accept().expect("Failed to accept client");
  }

  assert_eq!(clients.len(), 10);
}

#[test]
fn localhost() {
  let mut lio = Lio::new(64).unwrap();
  let (listener, addr) = setup_listener_v4();
  let client_sock = tcp_socket_v4();

  let (sender, receiver) = mpsc::channel();
  api::connect(&client_sock, addr).with_lio(&mut lio).send_with(sender);
  poll_until_recv(&mut lio, &receiver).expect("Failed to connect to localhost");

  let (_accepted, _) =
    listener.accept().expect("Failed to accept connected client");

  assert!(as_tcp_stream(&client_sock).peer_addr().unwrap().ip().is_loopback());
}
