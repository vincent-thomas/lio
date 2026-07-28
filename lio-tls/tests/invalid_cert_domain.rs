mod common;

use std::{io, net::TcpListener, thread};

use lio::{Lio, install_global, net::TcpStream};
use lio_tls::TlsConnector;

fn spawn_accepting_rustls_server(
  server_cfg: std::sync::Arc<rustls::ServerConfig>,
) -> io::Result<(std::net::SocketAddr, thread::JoinHandle<io::Result<()>>)> {
  let listener = TcpListener::bind("127.0.0.1:0")?;
  let addr = listener.local_addr()?;
  let server = thread::spawn(move || {
    let (mut tcp, _) = listener.accept()?;
    let mut conn =
      rustls::ServerConnection::new(server_cfg).map_err(io::Error::other)?;
    let _ = conn.complete_io(&mut tcp);
    Ok(())
  });
  Ok((addr, server))
}

#[test]
fn client_rejects_untrusted_server_certificate() -> io::Result<()> {
  let lio = Lio::new(64)?;
  let _guard = install_global(lio.clone());
  let (server_cfg, _) = common::configs();
  let client_cfg = common::client_config_without_trusting_server_cert();
  let (addr, server) = spawn_accepting_rustls_server(server_cfg)?;

  let err = match common::block_on(&lio, async {
    let tcp = TcpStream::connect(addr).await?;
    TlsConnector::new(client_cfg).connect("localhost", tcp).await
  }) {
    Ok(_) => panic!("TLS connection unexpectedly succeeded"),
    Err(err) => err,
  };

  assert_eq!(err.kind(), io::ErrorKind::Other);
  common::join_io(server)
}

#[test]
fn client_rejects_certificate_for_wrong_dns_name() -> io::Result<()> {
  let lio = Lio::new(64)?;
  let _guard = install_global(lio.clone());
  let (server_cfg, client_cfg) = common::configs();
  let (addr, server) = spawn_accepting_rustls_server(server_cfg)?;

  let err = match common::block_on(&lio, async {
    let tcp = TcpStream::connect(addr).await?;
    TlsConnector::new(client_cfg).connect("not-localhost.example", tcp).await
  }) {
    Ok(_) => panic!("TLS connection unexpectedly succeeded"),
    Err(err) => err,
  };

  assert_eq!(err.kind(), io::ErrorKind::Other);
  common::join_io(server)
}

#[test]
fn client_rejects_syntactically_invalid_dns_name() -> io::Result<()> {
  let lio = Lio::new(64)?;
  let _guard = install_global(lio.clone());
  let (_, client_cfg) = common::configs();

  let listener = TcpListener::bind("127.0.0.1:0")?;
  let addr = listener.local_addr()?;
  let server = thread::spawn(move || {
    let (_tcp, _) = listener.accept()?;
    Ok(())
  });

  let err = match common::block_on(&lio, async {
    let tcp = TcpStream::connect(addr).await?;
    TlsConnector::new(client_cfg).connect("", tcp).await
  }) {
    Ok(_) => panic!("TLS connection unexpectedly succeeded"),
    Err(err) => err,
  };

  assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
  common::join_io(server)
}
