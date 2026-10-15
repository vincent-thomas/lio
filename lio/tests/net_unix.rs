#![cfg(all(feature = "high", unix))]

mod common;

use std::{io, path::PathBuf};

use common::poll_recv;
use lio::{
  Lio,
  net::{UnixListener, UnixStream},
};

struct SocketPath(PathBuf);

impl SocketPath {
  fn new(name: &str) -> Self {
    let path = std::env::temp_dir()
      .join(format!("lio-{name}-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&path);
    Self(path)
  }
}

impl Drop for SocketPath {
  fn drop(&mut self) {
    let _ = std::fs::remove_file(&self.0);
  }
}

#[test]
fn stream_round_trip() {
  let mut lio = Lio::new(64).unwrap();
  let path = SocketPath::new("round-trip");

  let mut listener = UnixListener::bind(&path.0).unwrap().with_lio(&lio).send();
  let listener = poll_recv(&mut lio, &mut listener).unwrap();

  let mut accept = listener.accept().with_lio(&lio).send();
  let mut connect = UnixStream::connect(&path.0).unwrap().with_lio(&lio).send();
  let client = poll_recv(&mut lio, &mut connect).unwrap();
  let server = poll_recv(&mut lio, &mut accept).unwrap();

  let mut send = client.send(b"ping".to_vec()).with_lio(&lio).send();
  let (sent, _) = poll_recv(&mut lio, &mut send);
  assert_eq!(sent.unwrap(), 4);

  let mut recv = server.recv(vec![0u8; 16]).with_lio(&lio).send();
  let (read, buf) = poll_recv(&mut lio, &mut recv);
  assert_eq!(&buf[..read.unwrap()], b"ping");
}

#[test]
fn bind_fails_when_path_exists() {
  let mut lio = Lio::new(64).unwrap();
  let path = SocketPath::new("addr-in-use");

  let mut first = UnixListener::bind(&path.0).unwrap().with_lio(&lio).send();
  let _first = poll_recv(&mut lio, &mut first).unwrap();

  let mut second = UnixListener::bind(&path.0).unwrap().with_lio(&lio).send();
  let err = poll_recv(&mut lio, &mut second).err().unwrap();
  assert_eq!(err.kind(), io::ErrorKind::AddrInUse);
}

#[test]
fn connect_to_missing_path_fails() {
  let mut lio = Lio::new(64).unwrap();
  let path = SocketPath::new("missing");

  let mut connect = UnixStream::connect(&path.0).unwrap().with_lio(&lio).send();
  let err = poll_recv(&mut lio, &mut connect).err().unwrap();
  assert_eq!(err.kind(), io::ErrorKind::NotFound);
}

#[test]
fn accept_unix_returns_the_connection_and_peer_address() {
  use std::io::{Read, Write};

  let mut lio = Lio::new(64).unwrap();
  let path = SocketPath::new("accept-unix");
  let mut listener = UnixListener::bind(&path.0).unwrap().with_lio(&lio).send();
  let listener = poll_recv(&mut lio, &mut listener).unwrap();

  let mut accept = lio::api::accept_unix(&listener).with_lio(&lio).send();
  let mut client = std::os::unix::net::UnixStream::connect(&path.0).unwrap();
  let (server, peer) = poll_recv(&mut lio, &mut accept).unwrap();
  // The client connected without binding.
  assert!(peer.is_unnamed());

  // The accepted connection is alive.
  let mut send =
    lio::api::write(&server, b"pong".to_vec()).with_lio(&lio).send();
  let (sent, _) = poll_recv(&mut lio, &mut send);
  assert_eq!(sent.unwrap(), 4);
  let mut buf = [0u8; 4];
  client.read_exact(&mut buf).unwrap();
  assert_eq!(&buf, b"pong");
  client.write_all(b"x").unwrap();
}

#[test]
fn accept_on_a_unix_listener_points_to_accept_unix() {
  let mut lio = Lio::new(64).unwrap();
  let path = SocketPath::new("accept-ip-only");
  let mut listener = UnixListener::bind(&path.0).unwrap().with_lio(&lio).send();
  let listener = poll_recv(&mut lio, &mut listener).unwrap();

  let mut accept = lio::api::accept(&listener).with_lio(&lio).send();
  let _client = std::os::unix::net::UnixStream::connect(&path.0).unwrap();
  let err = poll_recv(&mut lio, &mut accept).unwrap_err();
  assert_eq!(err.kind(), io::ErrorKind::Unsupported);
  assert!(err.to_string().contains("accept_unix"));
}
