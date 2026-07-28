mod common;

use std::{
  io::{self, Read, Write},
  net::TcpStream as StdTcpStream,
  thread,
};

use lio::{Lio, install_global, net::TcpListener};
use lio_tls::TlsAcceptor;
use rustls::pki_types::ServerName;

#[test]
fn rustls_client_interoperates_with_lio_tls_server() -> io::Result<()> {
  let lio = Lio::new(64)?;
  let _guard = install_global(lio.clone());
  let (server_cfg, client_cfg) = common::configs();

  let listener = common::block_on(&lio, async {
    TcpListener::bind("127.0.0.1:0".parse().unwrap()).await
  })?;
  let addr = listener.local_addr()?;

  let client = thread::spawn(move || {
    let tcp = StdTcpStream::connect(addr)?;
    let conn = rustls::ClientConnection::new(
      client_cfg,
      ServerName::try_from("localhost").unwrap(),
    )
    .map_err(io::Error::other)?;
    let mut tls = rustls::StreamOwned::new(conn, tcp);
    tls.write_all(common::CLIENT_PING)?;
    tls.flush()?;

    let mut buf = [0; 64];
    let n = tls.read(&mut buf)?;
    assert_eq!(&buf[..n], common::SERVER_PONG);
    Ok(())
  });

  common::block_on(&lio, async {
    let (tcp, _) = listener.accept().await?;
    let mut tls = TlsAcceptor::new(server_cfg).accept(tcp).await?;
    let (buf, n) = tls.recv(vec![0; 64]).await?;
    assert_eq!(&buf[..n], common::CLIENT_PING);
    let (_, n) = tls.send(common::SERVER_PONG.to_vec()).await?;
    assert_eq!(n, common::SERVER_PONG.len());
    io::Result::Ok(())
  })?;

  common::join_io(client)
}
