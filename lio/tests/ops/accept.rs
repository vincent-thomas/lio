#![allow(
  clippy::duplicate_mod,
  clippy::unnecessary_mut_passed,
  clippy::expect_fun_call
)]

use super::common::{poll_until_recv, raw_id, socket_resource};
use lio::api::resource::Resource;
use lio::{Lio, api};
use std::future::IntoFuture;
use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::thread;

fn setup_listener() -> (Resource, std::net::SocketAddr) {
  let listener =
    TcpListener::bind("127.0.0.1:0").expect("Failed to bind TCP listener");
  listener.set_nonblocking(true).expect("Failed to set listener non-blocking");
  let addr = listener.local_addr().expect("Failed to query listener address");
  (socket_resource(listener), addr)
}

#[test]
fn basic() {
  let mut lio = Lio::new(64).unwrap();
  let (listener, addr) = setup_listener();

  let (sender_accept, receiver_accept) = mpsc::channel();
  api::accept(&listener).with_lio(&mut lio).send_with(sender_accept);

  let client = thread::spawn(move || {
    TcpStream::connect(addr).expect("Failed to connect client")
  });

  let (accepted_fd, _) =
    poll_until_recv(&mut lio, &receiver_accept).expect("Failed to accept");
  let _ = raw_id(&accepted_fd);

  let mut client = client.join().expect("client thread panicked");
  client.write_all(b"ping").expect("Failed to write from client");

  let (sender_recv, receiver_recv) = mpsc::channel();
  api::recv(&accepted_fd, vec![0u8; 4], None)
    .with_lio(&mut lio)
    .send_with(sender_recv);
  let (res, buf) = poll_until_recv(&mut lio, &receiver_recv);
  assert_eq!(res.expect("recv failed"), 4);
  assert_eq!(&buf[..4], b"ping");
}

#[test]
fn multiple() {
  let mut lio = Lio::new(64).unwrap();
  let (listener, addr) = setup_listener();

  for _ in 0..5 {
    let (sender_accept, receiver_accept) = mpsc::channel();
    api::accept(&listener).with_lio(&mut lio).send_with(sender_accept);

    let client = thread::spawn(move || {
      TcpStream::connect(addr).expect("Failed to connect client")
    });
    let (accepted_fd, _) =
      poll_until_recv(&mut lio, &receiver_accept).expect("Failed to accept");
    let _ = raw_id(&accepted_fd);
    client.join().expect("client thread panicked");
  }
}

#[test]
fn with_client_info() {
  let mut lio = Lio::new(64).unwrap();
  let (listener, addr) = setup_listener();

  let (sender_accept, receiver_accept) = mpsc::channel();
  api::accept(&listener).with_lio(&mut lio).send_with(sender_accept);
  let client = thread::spawn(move || {
    TcpStream::connect(addr).expect("Failed to connect client")
  });

  let (accepted_fd, client_addr) =
    poll_until_recv(&mut lio, &receiver_accept).expect("Failed to accept");
  let _ = raw_id(&accepted_fd);
  assert_eq!(
    client_addr.ip(),
    std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
  );
  client.join().expect("client thread panicked");
}

#[test]
fn ipv6() {
  let mut lio = Lio::new(64).unwrap();
  let listener =
    TcpListener::bind("[::1]:0").expect("Failed to bind IPv6 listener");
  listener
    .set_nonblocking(true)
    .expect("Failed to set IPv6 listener non-blocking");
  let addr =
    listener.local_addr().expect("Failed to query IPv6 listener address");
  let listener = socket_resource(listener);

  let (sender_accept, receiver_accept) = mpsc::channel();
  api::accept(&listener).with_lio(&mut lio).send_with(sender_accept);
  let client = thread::spawn(move || {
    TcpStream::connect(addr).expect("Failed to connect IPv6 client")
  });

  let (accepted_fd, client_addr) =
    poll_until_recv(&mut lio, &receiver_accept).expect("Failed to accept IPv6");
  let _ = raw_id(&accepted_fd);
  assert!(client_addr.is_ipv6());
  client.join().expect("client thread panicked");
}

#[test]
fn dropped_accept_frees_its_slot() {
  // An accept with no client never completes on its own.
  let lio = Lio::new(1).unwrap();
  let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
  listener.set_nonblocking(true).unwrap();
  let listener = socket_resource(listener);

  let mut accept: std::pin::Pin<Box<_>> =
    Box::pin(api::accept(&listener).with_lio(&lio).into_future());
  let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
  assert!(accept.as_mut().poll(&mut cx).is_pending());
  lio.run_timeout(std::time::Duration::from_millis(10)).unwrap();
  assert!(accept.as_mut().poll(&mut cx).is_pending());
  drop(accept);

  for _ in 0..200 {
    if !lio.is_full() {
      return;
    }
    lio.run_timeout(std::time::Duration::from_millis(5)).unwrap();
  }
  panic!("the cancelled accept never released its slot");
}
