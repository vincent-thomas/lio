mod common;

use std::{
  io::{self, Read, Write},
  net::TcpListener,
  thread,
};

use lio::{Lio, install_global, net::TcpStream};
use lio_tls::TlsConnector;

#[test]
fn lio_tls_client_interoperates_with_rustls_server() -> io::Result<()> {
  let lio = Lio::new(64)?;
  let _guard = install_global(lio.clone());
  let (server_cfg, client_cfg) = common::configs();

  let listener = TcpListener::bind("127.0.0.1:0")?;
  let addr = listener.local_addr()?;
  let server = thread::spawn(move || {
    let (tcp, _) = listener.accept()?;
    let conn =
      rustls::ServerConnection::new(server_cfg).map_err(io::Error::other)?;
    let mut tls = rustls::StreamOwned::new(conn, tcp);

    let mut buf = [0; 64];
    let n = tls.read(&mut buf)?;
    assert_eq!(&buf[..n], common::CLIENT_PING);
    tls.write_all(common::SERVER_PONG)?;
    tls.flush()?;
    Ok(())
  });

  common::block_on(&lio, async {
    let tcp = TcpStream::connect(addr).await?;
    let mut tls =
      TlsConnector::new(client_cfg).connect("localhost", tcp).await?;

    let (_, n) = tls.send(common::CLIENT_PING.to_vec()).await?;
    assert_eq!(n, common::CLIENT_PING.len());

    let (buf, n) = tls.recv(vec![0; 64]).await?;
    assert_eq!(&buf[..n], common::SERVER_PONG);
    io::Result::Ok(())
  })?;

  common::join_io(server)
}
