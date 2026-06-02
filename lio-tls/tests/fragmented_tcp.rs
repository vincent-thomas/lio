mod common;

use std::{
  io::{self, Read, Write},
  net::{TcpListener, TcpStream},
  thread,
};

use lio::{Lio, install_global, net::TcpStream as LioTcpStream};
use lio_tls::TlsConnector;

struct OneByteIo(TcpStream);

impl Read for OneByteIo {
  fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
    let len = buf.len().min(1);
    self.0.read(&mut buf[..len])
  }
}

impl Write for OneByteIo {
  fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
    self.0.write(&buf[..buf.len().min(1)])
  }

  fn flush(&mut self) -> io::Result<()> {
    self.0.flush()
  }
}

#[test]
fn client_handles_tls_bytes_arriving_one_tcp_byte_at_a_time() -> io::Result<()>
{
  let lio = Lio::new(64)?;
  let _guard = install_global(lio.clone());
  let (server_cfg, client_cfg) = common::configs();

  let listener = TcpListener::bind("127.0.0.1:0")?;
  let addr = listener.local_addr()?;
  let server = thread::spawn(move || {
    let (tcp, _) = listener.accept()?;
    tcp.set_nodelay(true)?;
    let conn =
      rustls::ServerConnection::new(server_cfg).map_err(io::Error::other)?;
    let mut tls = rustls::StreamOwned::new(conn, OneByteIo(tcp));

    let mut buf = [0; 64];
    let n = tls.read(&mut buf)?;
    assert_eq!(&buf[..n], common::CLIENT_PING);
    tls.write_all(common::SERVER_PONG)?;
    tls.flush()?;
    Ok(())
  });

  common::block_on(&lio, async {
    let tcp = LioTcpStream::connect(addr).await?;
    let mut tls =
      TlsConnector::new(client_cfg).connect("localhost", tcp).await?;
    tls.send(common::CLIENT_PING.to_vec()).await?;
    let (buf, n) = tls.recv(vec![0; 64]).await?;
    assert_eq!(&buf[..n], common::SERVER_PONG);
    io::Result::Ok(())
  })?;

  common::join_io(server)
}
