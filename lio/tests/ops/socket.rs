#![allow(
  clippy::duplicate_mod,
  clippy::unnecessary_mut_passed,
  clippy::expect_fun_call
)]

use super::common::{SocketKind, as_tcp_stream, raw_id, socket_kind};
use lio::{Lio, api};
use std::{sync::mpsc, thread, time::Duration};

type SocketResult = std::io::Result<lio::api::resource::Resource>;

/// Helper to poll until we receive a result
fn poll_until_recv<T>(lio: &mut Lio, receiver: &mpsc::Receiver<T>) -> T {
  let mut attempts = 0;
  loop {
    lio.try_run().unwrap();
    match receiver.try_recv() {
      Ok(result) => return result,
      Err(mpsc::TryRecvError::Empty) => {
        attempts += 1;
        if attempts > 10 {
          panic!("Operation did not complete after 10 attempts");
        }
        thread::sleep(Duration::from_micros(100));
      }
      Err(mpsc::TryRecvError::Disconnected) => {
        panic!("Channel disconnected");
      }
    }
  }
}

#[test]
fn test_socket_simple() {
  let mut lio = Lio::new(64).unwrap();

  let (sender, receiver) = mpsc::channel::<SocketResult>();

  api::socket(
    api::SockDomain::IPV4,
    api::SockType::STREAM,
    api::SockProto::DEFAULT,
  )
  .with_lio(&mut lio)
  .send_with(sender.clone());

  let sock = poll_until_recv(&mut lio, &receiver)
    .expect("Failed to create TCP IPv4 socket");

  assert_eq!(socket_kind(&sock), SocketKind::Stream);

  assert!(sock.will_close());

  drop(sock);
}

#[test]
fn test_socket_tcp_ipv4() {
  let mut lio = Lio::new(64).unwrap();

  let (sender, receiver) = mpsc::channel::<SocketResult>();

  api::socket(
    api::SockDomain::IPV4,
    api::SockType::STREAM,
    api::SockProto::DEFAULT,
  )
  .with_lio(&mut lio)
  .send_with(sender);

  let sock = poll_until_recv(&mut lio, &receiver)
    .expect("Failed to create TCP IPv4 socket");
  let _ = raw_id(&sock);

  assert_eq!(socket_kind(&sock), SocketKind::Stream);
}

#[test]
fn test_socket_tcp_ipv6() {
  let mut lio = Lio::new(64).unwrap();

  let (sender, receiver) = mpsc::channel::<SocketResult>();

  api::socket(
    api::SockDomain::IPV6,
    api::SockType::STREAM,
    api::SockProto::DEFAULT,
  )
  .with_lio(&mut lio)
  .send_with(sender);

  let sock = poll_until_recv(&mut lio, &receiver)
    .expect("Failed to create TCP IPv6 socket");
  let _ = raw_id(&sock);

  assert_eq!(socket_kind(&sock), SocketKind::Stream);
}

#[test]
fn test_socket_udp_ipv4() {
  let mut lio = Lio::new(64).unwrap();

  let (sender, receiver) = mpsc::channel::<SocketResult>();

  api::socket(
    api::SockDomain::IPV4,
    api::SockType::DGRAM,
    api::SockProto::DEFAULT,
  )
  .with_lio(&mut lio)
  .send_with(sender);

  let sock = poll_until_recv(&mut lio, &receiver)
    .expect("Failed to create UDP IPv4 socket");
  let _ = raw_id(&sock);

  assert_eq!(socket_kind(&sock), SocketKind::Datagram);
}

#[test]
fn test_socket_udp_ipv6() {
  let mut lio = Lio::new(64).unwrap();

  let (sender, receiver) = mpsc::channel::<SocketResult>();

  api::socket(
    api::SockDomain::IPV6,
    api::SockType::DGRAM,
    api::SockProto::DEFAULT,
  )
  .with_lio(&mut lio)
  .send_with(sender);

  let sock = poll_until_recv(&mut lio, &receiver)
    .expect("Failed to create UDP IPv6 socket");
  let _ = raw_id(&sock);

  assert_eq!(socket_kind(&sock), SocketKind::Datagram);
}

#[test]
fn test_socket_without_protocol() {
  let mut lio = Lio::new(64).unwrap();

  let (sender, receiver) = mpsc::channel::<SocketResult>();

  api::socket(
    api::SockDomain::IPV4,
    api::SockType::STREAM,
    api::SockProto::DEFAULT,
  )
  .with_lio(&mut lio)
  .send_with(sender);

  let sock = poll_until_recv(&mut lio, &receiver)
    .expect("Failed to create socket without explicit protocol");
  let _ = raw_id(&sock);
  // sock is automatically closed when dropped
}

#[test]
fn test_socket_unix_stream() {
  let mut lio = Lio::new(64).unwrap();
  let mut sock_recv = api::socket(
    api::SockDomain::UNIX,
    api::SockType::STREAM,
    api::SockProto::DEFAULT,
  )
  .with_lio(&mut lio)
  .send();

  assert_eq!(lio.try_run().unwrap(), 1);

  let socket = sock_recv.try_recv().unwrap().unwrap();
  let _ = raw_id(&socket);

  assert_eq!(socket_kind(&socket), SocketKind::Stream);
}

// Windows has no AF_UNIX datagram sockets.
#[cfg(unix)]
#[test]
fn test_socket_unix_dgram() {
  let mut lio = Lio::new(64).unwrap();
  let mut sock_recv = api::socket(
    api::SockDomain::UNIX,
    api::SockType::DGRAM,
    api::SockProto::DEFAULT,
  )
  .with_lio(&mut lio)
  .send();

  assert_eq!(lio.try_run().unwrap(), 1);

  let sock = sock_recv.try_recv().unwrap().unwrap();
  let _ = raw_id(&sock);

  assert_eq!(socket_kind(&sock), SocketKind::Datagram);
}

#[test]
fn test_socket_multiple() {
  let mut lio = Lio::new(64).unwrap();

  let (sender, receiver) = mpsc::channel::<SocketResult>();

  // Create multiple sockets
  api::socket(
    api::SockDomain::IPV4,
    api::SockType::STREAM,
    api::SockProto::DEFAULT,
  )
  .with_lio(&mut lio)
  .send_with(sender.clone());

  api::socket(
    api::SockDomain::IPV4,
    api::SockType::STREAM,
    api::SockProto::DEFAULT,
  )
  .with_lio(&mut lio)
  .send_with(sender.clone());

  api::socket(
    api::SockDomain::IPV4,
    api::SockType::DGRAM,
    api::SockProto::DEFAULT,
  )
  .with_lio(&mut lio)
  .send_with(sender.clone());

  let sock1 = poll_until_recv(&mut lio, &receiver)
    .expect("Failed to create first socket");
  let sock2 = poll_until_recv(&mut lio, &receiver)
    .expect("Failed to create second socket");
  let sock3 = poll_until_recv(&mut lio, &receiver)
    .expect("Failed to create third socket");

  let _ = raw_id(&sock1);
  let _ = raw_id(&sock2);
  let _ = raw_id(&sock3);
  assert_ne!(raw_id(&sock1), raw_id(&sock2));
  assert_ne!(raw_id(&sock1), raw_id(&sock3));
  assert_ne!(raw_id(&sock2), raw_id(&sock3));
  // sockets are automatically closed when dropped
}

#[test]
fn test_socket_concurrent() {
  let mut lio = Lio::new(64).unwrap();

  let (sender, receiver) = mpsc::channel::<SocketResult>();

  // Test creating multiple sockets
  for i in 0..20 {
    // Alternate between TCP IPv4, TCP IPv6, UDP IPv4, UDP IPv6
    match i % 4 {
      0 => api::socket(
        api::SockDomain::IPV4,
        api::SockType::STREAM,
        api::SockProto::DEFAULT,
      )
      .with_lio(&mut lio)
      .send_with(sender.clone()),
      1 => api::socket(
        api::SockDomain::IPV6,
        api::SockType::STREAM,
        api::SockProto::DEFAULT,
      )
      .with_lio(&mut lio)
      .send_with(sender.clone()),
      2 => api::socket(
        api::SockDomain::IPV4,
        api::SockType::DGRAM,
        api::SockProto::DEFAULT,
      )
      .with_lio(&mut lio)
      .send_with(sender.clone()),
      _ => api::socket(
        api::SockDomain::IPV6,
        api::SockType::DGRAM,
        api::SockProto::DEFAULT,
      )
      .with_lio(&mut lio)
      .send_with(sender.clone()),
    };
  }

  for _ in 0..20 {
    let sock =
      poll_until_recv(&mut lio, &receiver).expect("Failed to create socket");
    let _ = raw_id(&sock);
    // sock is automatically closed when dropped at end of loop iteration
  }
}

#[test]
fn test_socket_options_after_creation() {
  let mut lio = Lio::new(64).unwrap();

  let (sender, receiver) = mpsc::channel::<SocketResult>();

  api::socket(
    api::SockDomain::IPV4,
    api::SockType::STREAM,
    api::SockProto::DEFAULT,
  )
  .with_lio(&mut lio)
  .send_with(sender);

  let sock =
    poll_until_recv(&mut lio, &receiver).expect("Failed to create socket");

  // Options can be set on a freshly created socket.
  let stream = as_tcp_stream(&sock);
  stream.set_nodelay(true).expect("Failed to set TCP_NODELAY");
  assert!(stream.nodelay().unwrap());
}

#[test]
fn test_socket_nonblocking() {
  let mut lio = Lio::new(64).unwrap();

  let (sender, receiver) = mpsc::channel::<SocketResult>();

  api::socket(
    api::SockDomain::IPV4,
    api::SockType::STREAM,
    api::SockProto::DEFAULT,
  )
  .with_lio(&mut lio)
  .send_with(sender);

  let sock =
    poll_until_recv(&mut lio, &receiver).expect("Failed to create socket");

  // Set non-blocking mode
  as_tcp_stream(&sock)
    .set_nonblocking(true)
    .expect("Failed to set non-blocking mode");

  #[cfg(unix)]
  {
    // SAFETY: `sock` is a live descriptor for the duration of the call.
    let flags = unsafe { libc::fcntl(raw_id(&sock) as i32, libc::F_GETFL, 0) };
    assert!(flags & libc::O_NONBLOCK != 0);
  }
}
