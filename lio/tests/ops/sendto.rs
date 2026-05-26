#![allow(
  clippy::duplicate_mod,
  clippy::unnecessary_mut_passed,
  clippy::expect_fun_call
)]

//! Tests for sendto operations.

use super::common::{
  TcpPair, get_bound_addr, poll_recv, poll_until_recv, setup_tcp_pair,
  udp_socket,
};
use lio::Lio;
use lio::api;
use lio::api::resource::Resource;
use std::os::fd::{AsFd, AsRawFd};
use std::net::{SocketAddr, UdpSocket};
use std::sync::mpsc;

struct UdpPair {
  sender: Resource,
  receiver: UdpSocket,
  receiver_addr: SocketAddr,
}

fn setup_udp_pair() -> UdpPair {
  let sender = udp_socket();
  let receiver = UdpSocket::bind("127.0.0.1:0").expect("failed to bind receiver socket");
  let receiver_addr =
    receiver.local_addr().expect("failed to query receiver address");

  let bind_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
  let std_sockaddr = match bind_addr {
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
      sender.as_fd().as_raw_fd(),
      (&std_sockaddr as *const libc::sockaddr_in).cast(),
      std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
    )
  };
  assert_eq!(
    bind_result,
    0,
    "failed to bind sender socket: {}",
    std::io::Error::last_os_error()
  );
  let _sender_addr = get_bound_addr(&sender);

  UdpPair { sender, receiver, receiver_addr }
}

fn recv_once(socket: &UdpSocket) -> (Vec<u8>, SocketAddr) {
  let mut buf = vec![0u8; 64 * 1024];
  let (n, addr) = socket.recv_from(&mut buf).expect("peer recv_from failed");
  buf.truncate(n);
  (buf, addr)
}

fn recv_all(fd: &Resource, len: usize) -> Vec<u8> {
  let mut out = vec![0u8; len];
  let mut read = 0usize;

  while read < len {
    let n = unsafe {
      libc::recv(
        fd.as_fd().as_raw_fd(),
        out[read..].as_mut_ptr().cast(),
        out.len() - read,
        0,
      )
    };
    assert!(n >= 0, "peer recv failed");
    if n == 0 {
      break;
    }
    read += n as usize;
  }

  out.truncate(read);
  out
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

#[test]
fn basic() {
  let mut lio = Lio::new(64).unwrap();
  let UdpPair { sender, receiver, receiver_addr } = setup_udp_pair();

  let data = b"Hello, Receiver!".to_vec();
  let mut sender_op =
    api::sendto(&sender, data.clone(), receiver_addr, None).with_lio(&mut lio).send();

  let (bytes_sent, returned_buf) = poll_recv(&mut lio, &mut sender_op);
  let bytes_sent = bytes_sent.expect("sendto failed") as usize;

  assert_eq!(bytes_sent, data.len());
  assert_eq!(returned_buf, data);

  let (received, _from_addr) = recv_once(&receiver);
  assert_eq!(received, data);
}

#[test]
fn multiple() {
  let mut lio = Lio::new(64).unwrap();
  let UdpPair { sender, receiver, receiver_addr } = setup_udp_pair();

  for i in 0..10 {
    let data = format!("Message {}", i).into_bytes();
    let expected = data.clone();

    let (sender_send, receiver_send) = mpsc::channel();
    api::sendto(&sender, data, receiver_addr, None)
      .with_lio(&mut lio)
      .send_with(sender_send);

    let (bytes_sent, returned_buf) = poll_until_recv(&mut lio, &receiver_send);
    let bytes_sent = bytes_sent.expect("sendto failed") as usize;

    assert_eq!(bytes_sent, expected.len());
    assert_eq!(returned_buf, expected);

    let (received, _from_addr) = recv_once(&receiver);
    assert_eq!(received, expected);
  }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
#[test]
fn with_flags() {
  let mut lio = Lio::new(64).unwrap();
  let UdpPair { sender, receiver, receiver_addr } = setup_udp_pair();

  let data = b"sendto with flags".to_vec();
  let expected = data.clone();

  let (sender_send, receiver_send) = mpsc::channel();
  api::sendto(&sender, data, receiver_addr, Some(libc::MSG_NOSIGNAL))
    .with_lio(&mut lio)
    .send_with(sender_send);

  let (bytes_sent, returned_buf) = poll_until_recv(&mut lio, &receiver_send);
  let bytes_sent = bytes_sent.expect("sendto with flags failed") as usize;

  assert_eq!(bytes_sent, expected.len());
  assert_eq!(returned_buf, expected);

  let (received, _from_addr) = recv_once(&receiver);
  assert_eq!(received, expected);
}

#[test]
fn large_data() {
  let mut lio = Lio::new(64).unwrap();
  let UdpPair { sender, receiver, receiver_addr } = setup_udp_pair();

  let data: Vec<u8> = (0..8192).map(|i| (i % 256) as u8).collect();
  let expected = data.clone();

  let mut sender_op =
    api::sendto(&sender, data, receiver_addr, None).with_lio(&mut lio).send();

  let (bytes_sent, returned_buf) = poll_recv(&mut lio, &mut sender_op);
  let bytes_sent = bytes_sent.expect("sendto large_data failed") as usize;

  assert_eq!(bytes_sent, expected.len());
  assert_eq!(returned_buf, expected);

  let (received, _from_addr) = recv_once(&receiver);
  assert_eq!(received, expected);
}

#[test]
fn tcp_stream() {
  let mut lio = Lio::new(64).unwrap();
  let TcpPair { server_sock: _, client_sock, accepted_fd } = setup_tcp_pair(&mut lio);

  let data = b"tcp sendto".to_vec();
  let peer_addr = get_peer_addr(&client_sock);
  let mut sender_op =
    api::sendto(&client_sock, data.clone(), peer_addr, None).with_lio(&mut lio).send();

  let (bytes_sent, returned_buf) = poll_recv(&mut lio, &mut sender_op);
  assert_eq!(returned_buf, data);

  match bytes_sent {
    Ok(n) => {
      assert_eq!(n as usize, data.len());
      assert_eq!(recv_all(&accepted_fd, data.len()), data);
    }
    Err(err) => {
      let errno = err.raw_os_error();
      assert!(
        errno == Some(libc::EISCONN),
        "unexpected sendto-on-TCP error: {err:?}"
      );
    }
  }
}
