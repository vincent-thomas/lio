#![allow(
  clippy::duplicate_mod,
  clippy::unnecessary_mut_passed,
  clippy::expect_fun_call
)]

//! Tests for recvfrom operations.

use super::common::{
  TcpPair, get_bound_addr, poll_recv, poll_until_recv, setup_tcp_pair,
  udp_socket,
};
use lio::Lio;
use lio::api;
use lio::api::resource::Resource;
use std::os::fd::{AsFd, AsRawFd, FromRawFd};
use std::net::{SocketAddr, UdpSocket};
use std::sync::mpsc;

struct UdpPair {
  receiver: Resource,
  sender: UdpSocket,
  sender_addr: SocketAddr,
}

fn setup_udp_pair() -> UdpPair {
  let receiver = udp_socket();
  let recv_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
  let std_sockaddr = match recv_addr {
    SocketAddr::V4(addr) => {
      let mut sockaddr: libc::sockaddr_in = unsafe { std::mem::zeroed() };
      #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly"
      ))]
      {
        sockaddr.sin_len = std::mem::size_of::<libc::sockaddr_in>() as u8;
      }
      sockaddr.sin_family = libc::AF_INET as libc::sa_family_t;
      sockaddr.sin_port = addr.port().to_be();
      sockaddr.sin_addr = libc::in_addr { s_addr: u32::from(*addr.ip()).to_be() };
      sockaddr
    }
    SocketAddr::V6(_) => unreachable!("IPv4-only test helper"),
  };
  let bind_result = unsafe {
    libc::bind(
      receiver.as_fd().as_raw_fd(),
      (&std_sockaddr as *const libc::sockaddr_in).cast(),
      std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
    )
  };
  assert_eq!(
    bind_result,
    0,
    "failed to bind receiver socket: {}",
    std::io::Error::last_os_error()
  );
  let recv_addr = get_bound_addr(&receiver);

  let sender = UdpSocket::bind("127.0.0.1:0").expect("failed to bind sender socket");
  let sender_addr = sender.local_addr().expect("failed to query sender address");
  sender
    .connect(recv_addr)
    .expect("failed to connect sender socket");

  UdpPair { receiver, sender, sender_addr }
}

fn send_all(fd: &Resource, data: &[u8]) {
  let mut written = 0usize;

  while written < data.len() {
    let n = unsafe {
      libc::send(
        fd.as_fd().as_raw_fd(),
        data[written..].as_ptr().cast(),
        data.len() - written,
        0,
      )
    };
    assert!(n >= 0, "peer send failed");
    written += n as usize;
  }
}

fn get_peer_addr(sock: &Resource) -> SocketAddr {
  unsafe {
    let mut addr_storage = std::mem::MaybeUninit::<libc::sockaddr_in>::zeroed();
    let mut addr_len =
      std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
    let result = libc::getpeername(
      sock.as_fd().as_raw_fd(),
      addr_storage.as_mut_ptr() as *mut libc::sockaddr,
      &mut addr_len,
    );
    assert_eq!(
      result,
      0,
      "getpeername failed: {}",
      std::io::Error::last_os_error()
    );
    let sockaddr_in = addr_storage.assume_init();
    let port = u16::from_be(sockaddr_in.sin_port);
    format!("127.0.0.1:{}", port).parse::<SocketAddr>().unwrap()
  }
}

#[cfg(unix)]
fn setup_unix_dgram_pair() -> (Resource, Resource) {
  let mut fds = [0; 2];
  let result =
    unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_DGRAM, 0, fds.as_mut_ptr()) };
  assert_eq!(
    result,
    0,
    "socketpair failed: {}",
    std::io::Error::last_os_error()
  );

  unsafe { (Resource::from_raw_fd(fds[0]), Resource::from_raw_fd(fds[1])) }
}

#[test]
fn basic() {
  let mut lio = Lio::new(64).unwrap();
  let UdpPair { receiver, sender, sender_addr } = setup_udp_pair();

  let data = b"Hello, Datagram!".to_vec();
  sender.send(&data).expect("peer send failed");

  let mut receiver_op =
    api::recvfrom(&receiver, vec![0u8; 1024], None).with_lio(&mut lio).send();

  let (bytes_received, received_buf, from_addr): (
    std::io::Result<i32>,
    Vec<u8>,
    Option<SocketAddr>,
  ) =
    poll_recv(&mut lio, &mut receiver_op);
  let bytes_received = bytes_received.expect("recvfrom failed") as usize;

  assert_eq!(bytes_received, data.len());
  assert_eq!(&received_buf[..bytes_received], data.as_slice());
  assert_eq!(from_addr, Some(sender_addr));
}

#[test]
fn multiple() {
  let mut lio = Lio::new(64).unwrap();
  let UdpPair { receiver, sender, sender_addr } = setup_udp_pair();

  for i in 0..10 {
    let data = format!("Datagram {}", i).into_bytes();
    sender.send(&data).expect("peer send failed");

    let (sender_recv, receiver_recv) = mpsc::channel();
    api::recvfrom(&receiver, vec![0u8; 64], None)
      .with_lio(&mut lio)
      .send_with(sender_recv);

    let (bytes_received, received_buf, from_addr): (
      std::io::Result<i32>,
      Vec<u8>,
      Option<SocketAddr>,
    ) =
      poll_until_recv(&mut lio, &receiver_recv);
    let bytes_received = bytes_received.expect("recvfrom failed") as usize;

    assert_eq!(bytes_received, data.len());
    assert_eq!(&received_buf[..bytes_received], data.as_slice());
    assert_eq!(from_addr, Some(sender_addr));
  }
}

#[test]
fn partial_buffer() {
  let mut lio = Lio::new(64).unwrap();
  let UdpPair { receiver, sender, sender_addr } = setup_udp_pair();

  let data = b"This datagram is larger than the receive buffer".to_vec();
  sender.send(&data).expect("peer send failed");

  let (sender_recv, receiver_recv) = mpsc::channel();
  api::recvfrom(&receiver, vec![0u8; 10], None)
    .with_lio(&mut lio)
    .send_with(sender_recv);

  let (bytes_received, received_buf, from_addr): (
    std::io::Result<i32>,
    Vec<u8>,
    Option<SocketAddr>,
  ) =
    poll_until_recv(&mut lio, &receiver_recv);
  let bytes_received = bytes_received.expect("recvfrom failed") as usize;

  assert!(bytes_received <= 10);
  assert_eq!(&received_buf[..bytes_received], &data[..bytes_received]);
  assert_eq!(from_addr, Some(sender_addr));
}

#[test]
fn with_flags() {
  let mut lio = Lio::new(64).unwrap();
  let UdpPair { receiver, sender, sender_addr } = setup_udp_pair();

  let data = b"flags datagram".to_vec();
  sender.send(&data).expect("peer send failed");

  let (sender_recv, receiver_recv) = mpsc::channel();
  api::recvfrom(&receiver, vec![0u8; 64], Some(0))
    .with_lio(&mut lio)
    .send_with(sender_recv);

  let (bytes_received, received_buf, from_addr): (
    std::io::Result<i32>,
    Vec<u8>,
    Option<SocketAddr>,
  ) =
    poll_until_recv(&mut lio, &receiver_recv);
  let bytes_received = bytes_received.expect("recvfrom with flags failed") as usize;

  assert_eq!(bytes_received, data.len());
  assert_eq!(&received_buf[..bytes_received], data.as_slice());
  assert_eq!(from_addr, Some(sender_addr));
}

#[test]
fn tcp_stream() {
  let mut lio = Lio::new(64).unwrap();
  let TcpPair { server_sock: _, client_sock, accepted_fd } = setup_tcp_pair(&mut lio);

  let data = b"tcp recvfrom".to_vec();
  send_all(&client_sock, &data);

  let mut receiver_op =
    api::recvfrom(&accepted_fd, vec![0u8; 1024], None).with_lio(&mut lio).send();

  let (bytes_received, received_buf, from_addr): (
    std::io::Result<i32>,
    Vec<u8>,
    Option<SocketAddr>,
  ) =
    poll_recv(&mut lio, &mut receiver_op);
  let bytes_received = bytes_received.expect("recvfrom failed on TCP") as usize;

  assert_eq!(bytes_received, data.len());
  assert_eq!(&received_buf[..bytes_received], data.as_slice());
  assert_eq!(from_addr, None);
}

#[cfg(unix)]
#[test]
fn unix_dgram() {
  let mut lio = Lio::new(64).unwrap();
  let (receiver, sender) = setup_unix_dgram_pair();

  let data = b"unix recvfrom".to_vec();
  send_all(&sender, &data);

  let mut receiver_op =
    api::recvfrom(&receiver, vec![0u8; 1024], None).with_lio(&mut lio).send();

  let (bytes_received, received_buf, from_addr): (
    std::io::Result<i32>,
    Vec<u8>,
    Option<SocketAddr>,
  ) =
    poll_recv(&mut lio, &mut receiver_op);
  let bytes_received = bytes_received.expect("recvfrom failed on Unix dgram") as usize;

  assert_eq!(bytes_received, data.len());
  assert_eq!(&received_buf[..bytes_received], data.as_slice());
  assert_eq!(from_addr, None);
}
