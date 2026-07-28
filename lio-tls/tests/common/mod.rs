#![allow(dead_code)]

use std::{
  future::Future,
  io,
  sync::Arc,
  task::{Context, Poll, RawWaker, RawWakerVTable, Waker},
  time::{Duration, Instant},
};

use lio::Lio;
use rustls::{
  RootCertStore,
  pki_types::{CertificateDer, PrivateKeyDer},
};

pub const CLIENT_PING: &[u8] = b"ping from client";
pub const SERVER_PONG: &[u8] = b"pong from server";

pub fn init_crypto() {
  let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}

pub fn generate_test_certs()
-> (Vec<CertificateDer<'static>>, PrivateKeyDer<'static>) {
  init_crypto();
  let cert =
    rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
  let key = PrivateKeyDer::Pkcs8(cert.key_pair.serialize_der().into());
  let cert = vec![CertificateDer::from(cert.cert.der().to_vec())];
  (cert, key)
}

pub fn configs() -> (Arc<rustls::ServerConfig>, Arc<rustls::ClientConfig>) {
  let (certs, key) = generate_test_certs();
  let root_cert = certs[0].clone();

  let server = rustls::ServerConfig::builder()
    .with_no_client_auth()
    .with_single_cert(certs, key)
    .unwrap();

  let mut roots = RootCertStore::empty();
  roots.add(root_cert).unwrap();
  let client = rustls::ClientConfig::builder()
    .with_root_certificates(roots)
    .with_no_client_auth();

  (Arc::new(server), Arc::new(client))
}

pub fn client_config_without_trusting_server_cert() -> Arc<rustls::ClientConfig>
{
  Arc::new(
    rustls::ClientConfig::builder()
      .with_root_certificates(RootCertStore::empty())
      .with_no_client_auth(),
  )
}

fn noop_waker() -> Waker {
  unsafe fn clone(_: *const ()) -> RawWaker {
    raw_waker()
  }
  unsafe fn wake(_: *const ()) {}
  unsafe fn wake_by_ref(_: *const ()) {}
  unsafe fn drop(_: *const ()) {}
  fn raw_waker() -> RawWaker {
    RawWaker::new(
      std::ptr::null(),
      &RawWakerVTable::new(clone, wake, wake_by_ref, drop),
    )
  }
  unsafe { Waker::from_raw(raw_waker()) }
}

pub fn block_on<T>(lio: &Lio, fut: impl Future<Output = T>) -> T {
  let waker = noop_waker();
  let mut cx = Context::from_waker(&waker);
  let mut fut = Box::pin(fut);
  let deadline = Instant::now() + Duration::from_secs(10);

  loop {
    if let Poll::Ready(value) = fut.as_mut().poll(&mut cx) {
      return value;
    }
    assert!(Instant::now() < deadline, "future timed out");
    lio.run_timeout(Duration::from_millis(5)).unwrap();
  }
}

pub fn join_io(
  handle: std::thread::JoinHandle<io::Result<()>>,
) -> io::Result<()> {
  handle.join().unwrap()
}
