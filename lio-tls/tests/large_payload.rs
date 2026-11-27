mod common;

use std::{
  io::{self, Read, Write},
  net::TcpListener,
  thread,
};

use lio::{Lio, install_global, net::TcpStream};
use lio_tls::TlsConnector;

#[test]
fn large_payload_round_trip_spans_many_tls_records() -> io::Result<()> {
  let lio = Lio::new(64)?;
  let _guard = install_global(lio.clone());
  let (server_cfg, client_cfg) = common::configs();
  let payload: Vec<u8> = (0..256 * 1024).map(|i| (i % 251) as u8).collect();
  let expected = payload.clone();

  let listener = TcpListener::bind("127.0.0.1:0")?;
  let addr = listener.local_addr()?;
  let server = thread::spawn(move || {
    let (tcp, _) = listener.accept()?;
    let conn =
      rustls::ServerConnection::new(server_cfg).map_err(io::Error::other)?;
    let mut tls = rustls::StreamOwned::new(conn, tcp);

    let mut got = vec![0; expected.len()];
    tls.read_exact(&mut got)?;
    assert_eq!(got, expected);
    tls.write_all(&got)?;
    tls.flush()?;
    Ok(())
  });

  common::block_on(&lio, async {
    let tcp = TcpStream::connect(addr).await?;
    let tls = TlsConnector::new(client_cfg).connect("localhost", tcp).await?;

    // One send takes all of it, past what rustls buffers at once.
    let (result, payload, mut tls) = tls.send(payload).await;
    assert_eq!(result?, payload.len());

    let mut got = Vec::with_capacity(payload.len());
    let mut buf = vec![0; 8192];
    while got.len() < payload.len() {
      let (result, back, back_tls) = tls.recv(buf).await;
      let n = result?;
      assert!(n > 0);
      got.extend_from_slice(&back[..n]);
      (buf, tls) = (back, back_tls);
    }
    assert_eq!(got, payload);
    io::Result::Ok(())
  })?;

  common::join_io(server)
}
