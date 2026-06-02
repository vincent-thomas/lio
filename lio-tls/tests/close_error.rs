mod common;

use std::{
  io::{self, Write},
  net::TcpListener,
  thread,
};

use lio::{Lio, install_global, net::TcpStream};
use lio_tls::TlsConnector;

#[test]
fn handshake_fails_if_peer_closes_tcp_before_tls_handshake() -> io::Result<()> {
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
    TlsConnector::new(client_cfg).connect("localhost", tcp).await
  }) {
    Ok(_) => panic!("TLS connection unexpectedly succeeded"),
    Err(err) => err,
  };

  assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
  common::join_io(server)
}

#[test]
fn recv_reports_eof_when_peer_closes_without_application_data() -> io::Result<()>
{
  let lio = Lio::new(64)?;
  let _guard = install_global(lio.clone());
  let (server_cfg, client_cfg) = common::configs();

  let listener = TcpListener::bind("127.0.0.1:0")?;
  let addr = listener.local_addr()?;
  let server = thread::spawn(move || {
    let (mut tcp, _) = listener.accept()?;
    let mut conn =
      rustls::ServerConnection::new(server_cfg).map_err(io::Error::other)?;
    while conn.is_handshaking() {
      conn.complete_io(&mut tcp)?;
    }
    conn.send_close_notify();
    conn.write_tls(&mut tcp)?;
    tcp.flush()?;
    Ok(())
  });

  let (buf, n) = common::block_on(&lio, async {
    let tcp = TcpStream::connect(addr).await?;
    let mut tls =
      TlsConnector::new(client_cfg).connect("localhost", tcp).await?;
    tls.recv(vec![0; 64]).await
  })?;

  assert_eq!(n, 0);
  assert_eq!(buf.len(), 64);
  common::join_io(server)
}
