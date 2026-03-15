//! Integration tests for lio-tls.
//!
//! These tests verify TLS client/server communication using self-signed certificates.

use std::io;
use std::net::SocketAddr;
use std::pin::pin;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use std::time::Duration;

use lio::net::{TcpListener, TcpSocket};
use lio::Lio;
use lio_tls::{ClientTlsStream, ServerTlsStream, TlsAcceptor, TlsConnector};
use rcgen::{CertifiedKey, generate_simple_self_signed};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::{ClientConfig, RootCertStore, ServerConfig};

/// A simple waker that does nothing (we poll manually)
struct NoopWaker;

impl Wake for NoopWaker {
  fn wake(self: Arc<Self>) {}
}

/// Drive a future to completion by polling lio in a loop.
fn block_on<F: std::future::Future>(lio: &mut Lio, fut: F) -> F::Output {
  let waker = Waker::from(Arc::new(NoopWaker));
  let mut cx = Context::from_waker(&waker);
  let mut fut = pin!(fut);

  let start = std::time::Instant::now();
  let timeout = Duration::from_secs(30);

  loop {
    match fut.as_mut().poll(&mut cx) {
      Poll::Ready(result) => return result,
      Poll::Pending => {
        lio.run_timeout(Duration::from_millis(10)).unwrap();

        if start.elapsed() > timeout {
          panic!("block_on timed out after {:?}", timeout);
        }
      }
    }
  }
}

/// Generate a self-signed certificate and key for testing.
fn generate_test_cert() -> CertifiedKey {
  generate_simple_self_signed(vec!["localhost".to_string()])
    .expect("failed to generate certificate")
}

/// Create a server TLS config from a certificate.
fn make_server_config(cert: &CertifiedKey) -> Arc<ServerConfig> {
  let cert_der = CertificateDer::from(cert.cert.der().to_vec());
  let key_der =
    PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der()));

  Arc::new(
    ServerConfig::builder()
      .with_no_client_auth()
      .with_single_cert(vec![cert_der], key_der)
      .expect("failed to create server config"),
  )
}

/// Create a client TLS config that trusts the given certificate.
fn make_client_config(cert: &CertifiedKey) -> Arc<ClientConfig> {
  let cert_der = CertificateDer::from(cert.cert.der().to_vec());

  let mut root_store = RootCertStore::empty();
  root_store.add(cert_der).expect("failed to add cert to root store");

  Arc::new(
    ClientConfig::builder()
      .with_root_certificates(root_store)
      .with_no_client_auth(),
  )
}

/// Setup TCP connections and perform TLS handshake.
fn setup_tls_pair_sync(
  lio: &mut Lio,
  server_config: Arc<ServerConfig>,
  client_config: Arc<ClientConfig>,
) -> io::Result<(ClientTlsStream, ServerTlsStream)> {
  // Create server listener
  let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
  let listener = block_on(lio, TcpListener::bind_async(addr))?;
  let server_addr = listener.local_addr()?;

  // Connect client and accept on server - these need to happen concurrently
  let waker = Waker::from(Arc::new(NoopWaker));
  let mut cx = Context::from_waker(&waker);

  let client_connect = TcpSocket::connect_async(server_addr);
  let server_accept = std::future::IntoFuture::into_future(listener.accept());

  let mut client_fut = pin!(client_connect);
  let mut server_fut = pin!(server_accept);

  let mut client_tcp: Option<io::Result<TcpSocket>> = None;
  let mut server_tcp: Option<io::Result<(TcpSocket, SocketAddr)>> = None;

  let start = std::time::Instant::now();
  let timeout = Duration::from_secs(10);

  // Drive both TCP futures
  while client_tcp.is_none() || server_tcp.is_none() {
    if client_tcp.is_none() {
      if let Poll::Ready(r) = client_fut.as_mut().poll(&mut cx) {
        client_tcp = Some(r);
      }
    }
    if server_tcp.is_none() {
      if let Poll::Ready(r) = server_fut.as_mut().poll(&mut cx) {
        server_tcp = Some(r);
      }
    }

    lio.run_timeout(Duration::from_millis(5)).unwrap();

    if start.elapsed() > timeout {
      return Err(io::Error::new(
        io::ErrorKind::TimedOut,
        "TCP connect/accept timed out",
      ));
    }
  }

  let client_tcp = client_tcp.unwrap()?;
  let (server_tcp, _) = server_tcp.unwrap()?;

  // Perform TLS handshakes
  let connector = TlsConnector::new(client_config);
  let acceptor = TlsAcceptor::new(server_config);

  let client_tls = connector.connect("localhost", client_tcp);
  let server_tls = acceptor.accept(server_tcp);

  let mut client_fut = pin!(client_tls);
  let mut server_fut = pin!(server_tls);

  let mut client_result: Option<io::Result<ClientTlsStream>> = None;
  let mut server_result: Option<io::Result<ServerTlsStream>> = None;

  let start = std::time::Instant::now();

  // Drive both TLS futures
  while client_result.is_none() || server_result.is_none() {
    if client_result.is_none() {
      if let Poll::Ready(r) = client_fut.as_mut().poll(&mut cx) {
        client_result = Some(r);
      }
    }
    if server_result.is_none() {
      if let Poll::Ready(r) = server_fut.as_mut().poll(&mut cx) {
        server_result = Some(r);
      }
    }

    lio.run_timeout(Duration::from_millis(5)).unwrap();

    if start.elapsed() > timeout {
      return Err(io::Error::new(
        io::ErrorKind::TimedOut,
        "TLS handshake timed out",
      ));
    }
  }

  let client = client_result.unwrap()?;
  let server = server_result.unwrap()?;

  Ok((client, server))
}

// ============================================================================
// Basic Tests
// ============================================================================

#[test]
fn test_tls_connector_creation() {
  let cert = generate_test_cert();
  let config = make_client_config(&cert);
  let _connector = TlsConnector::new(config);
}

#[test]
fn test_tls_acceptor_creation() {
  let cert = generate_test_cert();
  let config = make_server_config(&cert);
  let _acceptor = TlsAcceptor::new(config);
}

#[test]
fn test_tls_handshake() {
  let mut lio = Lio::new(64).unwrap();
  lio::install_global(lio.clone());

  let cert = generate_test_cert();
  let server_config = make_server_config(&cert);
  let client_config = make_client_config(&cert);

  let result = setup_tls_pair_sync(&mut lio, server_config, client_config);

  lio::uninstall_global();

  assert!(result.is_ok(), "TLS handshake failed: {:?}", result.err());
}

#[test]
fn test_tls_send_recv_basic() {
  let mut lio = Lio::new(64).unwrap();
  lio::install_global(lio.clone());

  let cert = generate_test_cert();
  let server_config = make_server_config(&cert);
  let client_config = make_client_config(&cert);

  let (mut client, mut server) =
    setup_tls_pair_sync(&mut lio, server_config, client_config)
      .expect("TLS setup failed");

  // Client sends data
  let data = b"Hello, TLS server!".to_vec();
  let result = block_on(&mut lio, client.send(data.clone()));
  let (_, bytes_sent) = result.expect("send failed");
  assert_eq!(bytes_sent, data.len());

  // Server receives data
  let buf = vec![0u8; 1024];
  let result = block_on(&mut lio, server.recv(buf));
  let (buf, bytes_recv) = result.expect("recv failed");
  assert_eq!(bytes_recv, data.len());
  assert_eq!(&buf[..bytes_recv], data.as_slice());

  lio::uninstall_global();
}

#[test]
fn test_tls_bidirectional_communication() {
  let mut lio = Lio::new(64).unwrap();
  lio::install_global(lio.clone());

  let cert = generate_test_cert();
  let server_config = make_server_config(&cert);
  let client_config = make_client_config(&cert);

  let (mut client, mut server) =
    setup_tls_pair_sync(&mut lio, server_config, client_config)
      .expect("TLS setup failed");

  // Client sends request
  let request = b"GET / HTTP/1.1\r\n\r\n".to_vec();
  block_on(&mut lio, client.send(request.clone())).expect("send failed");

  // Server receives request
  let buf = vec![0u8; 1024];
  let (buf, n) = block_on(&mut lio, server.recv(buf)).expect("recv failed");
  assert_eq!(&buf[..n], request.as_slice());

  // Server sends response
  let response = b"HTTP/1.1 200 OK\r\n\r\nHello!".to_vec();
  block_on(&mut lio, server.send(response.clone())).expect("send failed");

  // Client receives response
  let buf = vec![0u8; 1024];
  let (buf, n) = block_on(&mut lio, client.recv(buf)).expect("recv failed");
  assert_eq!(&buf[..n], response.as_slice());

  lio::uninstall_global();
}

#[test]
fn test_tls_multiple_messages() {
  let mut lio = Lio::new(64).unwrap();
  lio::install_global(lio.clone());

  let cert = generate_test_cert();
  let server_config = make_server_config(&cert);
  let client_config = make_client_config(&cert);

  let (mut client, mut server) =
    setup_tls_pair_sync(&mut lio, server_config, client_config)
      .expect("TLS setup failed");

  // Send multiple messages
  for i in 0..10 {
    let msg = format!("Message {}", i).into_bytes();
    let msg_len = msg.len();

    block_on(&mut lio, client.send(msg.clone())).expect("send failed");

    let buf = vec![0u8; 256];
    let (buf, n) = block_on(&mut lio, server.recv(buf)).expect("recv failed");
    assert_eq!(n, msg_len);
    assert_eq!(&buf[..n], msg.as_slice());
  }

  lio::uninstall_global();
}

#[test]
fn test_tls_large_data() {
  let mut lio = Lio::new(64).unwrap();
  lio::install_global(lio.clone());

  let cert = generate_test_cert();
  let server_config = make_server_config(&cert);
  let client_config = make_client_config(&cert);

  let (mut client, mut server) =
    setup_tls_pair_sync(&mut lio, server_config, client_config)
      .expect("TLS setup failed");

  // Send 1KB of data (small enough to fit in a single TLS record)
  let large_data: Vec<u8> = (0..1024).map(|i| (i % 256) as u8).collect();
  let data_len = large_data.len();

  block_on(&mut lio, client.send(large_data.clone())).expect("send failed");

  // Receive data
  let buf = vec![0u8; 8192];
  let (buf, n) = block_on(&mut lio, server.recv(buf)).expect("recv failed");

  assert_eq!(n, data_len);
  assert_eq!(&buf[..n], large_data.as_slice());

  lio::uninstall_global();
}

#[test]
fn test_tls_echo_server() {
  let mut lio = Lio::new(64).unwrap();
  lio::install_global(lio.clone());

  let cert = generate_test_cert();
  let server_config = make_server_config(&cert);
  let client_config = make_client_config(&cert);

  let (mut client, mut server) =
    setup_tls_pair_sync(&mut lio, server_config, client_config)
      .expect("TLS setup failed");

  // Echo test: client sends, server echoes back
  for i in 0..5 {
    let msg = format!("Echo test {}", i).into_bytes();
    let msg_len = msg.len();

    // Client sends
    block_on(&mut lio, client.send(msg.clone())).expect("send failed");

    // Server receives
    let buf = vec![0u8; 256];
    let (buf, n) = block_on(&mut lio, server.recv(buf)).expect("recv failed");
    assert_eq!(n, msg_len);

    // Server echoes
    let echo = buf[..n].to_vec();
    block_on(&mut lio, server.send(echo)).expect("send failed");

    // Client receives echo
    let buf = vec![0u8; 256];
    let (buf, n) = block_on(&mut lio, client.recv(buf)).expect("recv failed");
    assert_eq!(n, msg_len);
    assert_eq!(&buf[..n], msg.as_slice());
  }

  lio::uninstall_global();
}

#[test]
fn test_tls_get_ref() {
  let mut lio = Lio::new(64).unwrap();
  lio::install_global(lio.clone());

  let cert = generate_test_cert();
  let server_config = make_server_config(&cert);
  let client_config = make_client_config(&cert);

  let (client, server) = setup_tls_pair_sync(&mut lio, server_config, client_config)
    .expect("TLS setup failed");

  // Verify we can get references to the underlying TCP sockets
  let _client_tcp: &TcpSocket = client.get_ref();
  let _server_tcp: &TcpSocket = server.get_ref();

  lio::uninstall_global();
}

// ============================================================================
// Stress Tests
// ============================================================================

#[test]
fn test_tls_many_small_messages() {
  let mut lio = Lio::new(64).unwrap();
  lio::install_global(lio.clone());

  let cert = generate_test_cert();
  let server_config = make_server_config(&cert);
  let client_config = make_client_config(&cert);

  let (mut client, mut server) =
    setup_tls_pair_sync(&mut lio, server_config, client_config)
      .expect("TLS setup failed");

  // Send 100 small messages
  for i in 0..100 {
    let msg = format!("{}", i).into_bytes();
    block_on(&mut lio, client.send(msg.clone())).expect("send failed");

    let buf = vec![0u8; 64];
    let (buf, n) = block_on(&mut lio, server.recv(buf)).expect("recv failed");
    assert_eq!(&buf[..n], msg.as_slice());
  }

  lio::uninstall_global();
}

#[test]
fn test_tls_alternating_send_recv() {
  let mut lio = Lio::new(64).unwrap();
  lio::install_global(lio.clone());

  let cert = generate_test_cert();
  let server_config = make_server_config(&cert);
  let client_config = make_client_config(&cert);

  let (mut client, mut server) =
    setup_tls_pair_sync(&mut lio, server_config, client_config)
      .expect("TLS setup failed");

  for i in 0..20 {
    if i % 2 == 0 {
      // Client -> Server
      let msg = format!("C->S: {}", i).into_bytes();
      block_on(&mut lio, client.send(msg.clone())).expect("send failed");
      let buf = vec![0u8; 64];
      let (buf, n) = block_on(&mut lio, server.recv(buf)).expect("recv failed");
      assert_eq!(&buf[..n], msg.as_slice());
    } else {
      // Server -> Client
      let msg = format!("S->C: {}", i).into_bytes();
      block_on(&mut lio, server.send(msg.clone())).expect("send failed");
      let buf = vec![0u8; 64];
      let (buf, n) = block_on(&mut lio, client.recv(buf)).expect("recv failed");
      assert_eq!(&buf[..n], msg.as_slice());
    }
  }

  lio::uninstall_global();
}

#[test]
fn test_tls_varying_message_sizes() {
  let mut lio = Lio::new(64).unwrap();
  lio::install_global(lio.clone());

  let cert = generate_test_cert();
  let server_config = make_server_config(&cert);
  let client_config = make_client_config(&cert);

  let (mut client, mut server) =
    setup_tls_pair_sync(&mut lio, server_config, client_config)
      .expect("TLS setup failed");

  // Send messages of varying sizes (keeping within single TLS record bounds)
  let sizes = [1, 10, 100, 500, 1000, 2000];

  for size in sizes {
    let msg: Vec<u8> = (0..size).map(|i| (i % 256) as u8).collect();
    block_on(&mut lio, client.send(msg.clone())).expect("send failed");

    let buf = vec![0u8; 4096];
    let (buf, n) = block_on(&mut lio, server.recv(buf)).expect("recv failed");

    assert_eq!(n, size, "size mismatch for message of size {}", size);
    assert_eq!(&buf[..n], msg.as_slice());
  }

  lio::uninstall_global();
}
