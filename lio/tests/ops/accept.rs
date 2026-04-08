use super::common::poll_until_recv;
use lio::api::resource::Resource;
use lio::{Lio, api};
use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::os::fd::{AsFd, AsRawFd, FromRawFd, IntoRawFd};
use std::sync::mpsc;
use std::thread;

fn setup_listener() -> (Resource, std::net::SocketAddr) {
  let listener =
    TcpListener::bind("127.0.0.1:0").expect("Failed to bind TCP listener");
  listener.set_nonblocking(true).expect("Failed to set listener non-blocking");
  let addr = listener.local_addr().expect("Failed to query listener address");
  unsafe { (Resource::from_raw_fd(listener.into_raw_fd()), addr) }
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
  assert!(accepted_fd.as_fd().as_raw_fd() >= 0);

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
    assert!(accepted_fd.as_fd().as_raw_fd() >= 0);
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
  assert!(accepted_fd.as_fd().as_raw_fd() >= 0);
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
  let listener = unsafe { Resource::from_raw_fd(listener.into_raw_fd()) };

  let (sender_accept, receiver_accept) = mpsc::channel();
  api::accept(&listener).with_lio(&mut lio).send_with(sender_accept);
  let client = thread::spawn(move || {
    TcpStream::connect(addr).expect("Failed to connect IPv6 client")
  });

  let (accepted_fd, client_addr) =
    poll_until_recv(&mut lio, &receiver_accept).expect("Failed to accept IPv6");
  assert!(accepted_fd.as_fd().as_raw_fd() >= 0);
  assert!(client_addr.is_ipv6());
  client.join().expect("client thread panicked");
}
