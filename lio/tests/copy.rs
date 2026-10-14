//! `lio::io::copy` / `copy_n` over real sockets.

mod common;

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

use lio::Lio;

/// A connected pair: `(std end, lio end)`.
fn tcp_pair() -> (TcpStream, lio::api::resource::Resource) {
  let listener = TcpListener::bind("127.0.0.1:0").unwrap();
  let ours = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
  let (theirs, _) = listener.accept().unwrap();
  (ours, common::socket_resource(theirs))
}

/// Reads what is available on `stream` within `wait`.
fn read_available(stream: &mut TcpStream, out: &mut Vec<u8>, wait: Duration) {
  stream.set_read_timeout(Some(wait)).unwrap();
  let mut buf = [0u8; 512];
  if let Ok(n) = stream.read(&mut buf) {
    out.extend_from_slice(&buf[..n]);
  }
}

#[test]
fn copy_n_stops_at_the_limit_after_a_short_read() {
  let lio = Lio::new(8).unwrap();
  let (mut src, src_res) = tcp_pair();
  let (mut dst, dst_res) = tcp_pair();

  // The first read comes back short of the limit.
  src.write_all(&[1; 60]).unwrap();
  let mut copy = lio::io::copy_n(&src_res, &dst_res, 100).with_lio(&lio).send();
  let mut copied = Vec::new();
  let deadline = Instant::now() + Duration::from_secs(5);
  while copied.len() < 60 {
    assert!(Instant::now() < deadline, "first chunk never arrived");
    lio.run_timeout(Duration::from_millis(10)).unwrap();
    read_available(&mut dst, &mut copied, Duration::from_millis(1));
  }

  // Far more than the remaining 40 bytes is now available to the next read.
  src.write_all(&[2; 200]).unwrap();
  let total = loop {
    assert!(Instant::now() < deadline, "copy never finished");
    lio.run_timeout(Duration::from_millis(10)).unwrap();
    if let Some(result) = copy.try_recv() {
      break result.unwrap();
    }
  };
  // Collect whatever else was written, including any overrun.
  loop {
    let before = copied.len();
    read_available(&mut dst, &mut copied, Duration::from_millis(50));
    if copied.len() == before {
      break;
    }
  }

  assert_eq!(total, 100);
  assert_eq!(copied.len(), 100);
  assert!(copied[..60].iter().all(|&b| b == 1));
  assert!(copied[60..].iter().all(|&b| b == 2));
}
