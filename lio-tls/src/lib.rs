//! TLS support for lio using rustls.
//!
//! Every TLS operation here is a lio [model](lio::model): the handshake,
//! [`send`](TlsStream::send) and [`recv`](TlsStream::recv) each return an
//! [`Io`], so they run every way an lio operation does. They can be
//! awaited, given a callback with [`when_done`](Io::when_done), sent to a
//! channel, bound to a driver with [`with_lio`](Io::with_lio), or polled
//! from a model of your own.
//!
//! An operation takes the [`TlsStream`] by value and hands it back with its
//! result, the way lio hands back a buffer. One session therefore has one
//! operation at a time, which TLS requires: two writes in flight together
//! would interleave their records. An operation dropped before it is done
//! drops the stream with it, since a record cut off half way has left the
//! session unusable.
//!
//! # Example (Client)
//!
//! ```no_run
//! use lio::net::TcpStream;
//! use lio_tls::TlsConnector;
//! use std::{net::SocketAddr, sync::Arc};
//!
//! async fn example() -> std::io::Result<()> {
//!     let connector = TlsConnector::new(Arc::new(client_config()));
//!     let addr: SocketAddr = "93.184.216.34:443".parse().unwrap();
//!     let tcp = TcpStream::connect(addr).await?;
//!     let tls = connector.connect("example.com", tcp).await?;
//!
//!     let request = b"GET / HTTP/1.1\r\nHost: example.com\r\n\r\n".to_vec();
//!     let (result, _request, tls) = tls.send(request).await;
//!     result?;
//!     let (result, buf, _tls) = tls.recv(vec![0u8; 4096]).await;
//!     let n = result?;
//!     println!("{}", String::from_utf8_lossy(&buf[..n]));
//!     Ok(())
//! }
//!
//! fn client_config() -> rustls::ClientConfig {
//!     // ... configure rustls
//!     todo!()
//! }
//! ```
//!
//! # Example (without async)
//!
//! ```no_run
//! use lio::Lio;
//! use lio_tls::ClientTlsStream;
//!
//! fn ping(lio: &Lio, tls: ClientTlsStream) {
//!     tls.send(b"ping".to_vec()).with_lio(lio).when_done(|(result, _, tls)| {
//!         if result.is_ok() {
//!             tls.recv(vec![0; 64]).when_done(|(result, buf, _tls)| {
//!                 if let Ok(n) = result {
//!                     println!("{}", String::from_utf8_lossy(&buf[..n]));
//!                 }
//!             });
//!         }
//!     });
//! }
//! ```

#![deny(clippy::unwrap_used, clippy::expect_used)]

use std::io::{self, Read, Write};
use std::ops::DerefMut;
use std::sync::Arc;

use lio::api::Io;
use lio::model::{Cx, OpModel, Step};
use lio::net::TcpStream;
use lio::ready;
use rustls::{
  ClientConfig, ClientConnection, ConnectionCommon, ServerConfig,
  ServerConnection, SideData,
};

pub mod ops;

use ops::{Handshake, Recv, Send};

/// How many bytes one receive from the socket asks for.
const RECV_SIZE: usize = 16 * 1024;

/// A TLS connector for establishing client TLS connections.
pub struct TlsConnector {
  config: Arc<ClientConfig>,
}

impl TlsConnector {
  /// Creates a new TLS connector with the given configuration.
  pub fn new(config: Arc<ClientConfig>) -> Self {
    Self { config }
  }

  /// Performs the client handshake over `stream`, for the server named
  /// `domain`.
  ///
  /// A `domain` that is not a valid DNS name or IP address fails the
  /// operation with [`io::ErrorKind::InvalidInput`], without any I/O.
  pub fn connect(
    &self,
    domain: &str,
    stream: TcpStream,
  ) -> Io<Handshake<ClientConnection>> {
    let conn = rustls::pki_types::ServerName::try_from(domain.to_string())
      .map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidInput, "invalid DNS name")
      })
      .and_then(|server_name| {
        ClientConnection::new(self.config.clone(), server_name)
          .map_err(io::Error::other)
      });
    Io::from_op(Handshake::new(conn.map(|conn| TlsStream::new(stream, conn))))
  }
}

/// A TLS acceptor for establishing server TLS connections.
pub struct TlsAcceptor {
  config: Arc<ServerConfig>,
}

impl TlsAcceptor {
  /// Creates a new TLS acceptor with the given configuration.
  pub fn new(config: Arc<ServerConfig>) -> Self {
    Self { config }
  }

  /// Performs the server handshake over `stream`.
  pub fn accept(&self, stream: TcpStream) -> Io<Handshake<ServerConnection>> {
    let conn =
      ServerConnection::new(self.config.clone()).map_err(io::Error::other);
    Io::from_op(Handshake::new(conn.map(|conn| TlsStream::new(stream, conn))))
  }
}

/// A TLS session over a TCP socket.
///
/// `C` is the rustls connection: [`ClientConnection`] for a
/// [`ClientTlsStream`], [`ServerConnection`] for a [`ServerTlsStream`].
pub struct TlsStream<C> {
  tcp: TcpStream,
  conn: C,
  /// Kept between sends and receives so that they do not allocate. Empty
  /// while an operation on the socket holds it.
  send_buf: Vec<u8>,
  recv_buf: Vec<u8>,
}

/// A TLS client stream.
pub type ClientTlsStream = TlsStream<ClientConnection>;

/// A TLS server stream.
pub type ServerTlsStream = TlsStream<ServerConnection>;

impl<C> TlsStream<C> {
  fn new(tcp: TcpStream, conn: C) -> Self {
    Self {
      tcp,
      conn,
      send_buf: Vec::new(),
      recv_buf: Vec::with_capacity(RECV_SIZE),
    }
  }

  /// Returns a reference to the underlying TCP socket.
  pub fn get_ref(&self) -> &TcpStream {
    &self.tcp
  }

  /// Returns a reference to the rustls connection, to read what the
  /// handshake agreed on (the protocol version, ALPN, the peer's
  /// certificates).
  pub fn connection(&self) -> &C {
    &self.conn
  }
}

impl<C, D> TlsStream<C>
where
  C: DerefMut<Target = ConnectionCommon<D>>,
  D: SideData,
{
  /// Sends all of `buf` as plaintext over the TLS session.
  ///
  /// The operation yields the number of bytes sent, `buf` and the stream.
  /// It is done once the encrypted bytes have been handed to the socket.
  pub fn send<B: AsRef<[u8]>>(self, buf: B) -> Io<Send<C, B>> {
    Io::from_op(Send::new(self, buf))
  }

  /// Receives plaintext into `buf`, filling it from the start.
  ///
  /// The operation yields the number of bytes received, `buf` and the
  /// stream. Zero bytes means the peer closed the session with a
  /// `close_notify` (or that `buf` is empty).
  pub fn recv<B: AsMut<[u8]>>(self, buf: B) -> Io<Recv<C, B>> {
    Io::from_op(Recv::new(self, buf))
  }
}

/// Lets `?`-style early returns work in a function that returns
/// `Step<io::Result<_>>`.
macro_rules! tri {
  ($result:expr) => {
    match $result {
      Ok(value) => value,
      Err(err) => return Step::Done(Err(err)),
    }
  };
}
/// A stream and the socket operation it has in flight, if any.
///
/// Only the models in [`ops`] hold one. They are done only when no socket
/// operation is in flight, so the stream they hand back is whole.
struct Session<C> {
  stream: TlsStream<C>,
  sending: Option<Io<lio::api::ops::Send<Vec<u8>>>>,
  receiving: Option<Io<lio::api::ops::Recv<Vec<u8>>>>,
}

impl<C, D> Session<C>
where
  C: DerefMut<Target = ConnectionCommon<D>>,
  D: SideData,
{
  fn new(stream: TlsStream<C>) -> Self {
    Self { stream, sending: None, receiving: None }
  }

  fn into_stream(self) -> TlsStream<C> {
    self.stream
  }

  /// Sends everything rustls has to send.
  fn poll_flush(&mut self, cx: &mut Cx<'_>) -> Step<io::Result<()>> {
    loop {
      if let Some(sending) = &mut self.sending {
        let (result, mut chunk) = ready!(sending.poll(cx));
        self.sending = None;
        match tri!(result) {
          0 => {
            return Step::Done(Err(io::Error::new(
              io::ErrorKind::WriteZero,
              "failed to write TLS bytes",
            )));
          }
          n if n < chunk.len() => {
            chunk.drain(..n);
            self.sending = Some(self.stream.tcp.send(chunk));
            continue;
          }
          _ => {
            chunk.clear();
            self.stream.send_buf = chunk;
          }
        }
      }
      if !self.stream.conn.wants_write() {
        return Step::Done(Ok(()));
      }
      let mut chunk = std::mem::take(&mut self.stream.send_buf);
      while self.stream.conn.wants_write() {
        tri!(self.stream.conn.write_tls(&mut chunk));
      }
      self.sending = Some(self.stream.tcp.send(chunk));
    }
  }

  /// Receives once from the socket and hands what came to rustls.
  fn poll_fill(&mut self, cx: &mut Cx<'_>) -> Step<io::Result<()>> {
    let receiving = self.receiving.get_or_insert_with(|| {
      let mut buf = std::mem::take(&mut self.stream.recv_buf);
      buf.clear();
      buf.reserve(RECV_SIZE);
      self.stream.tcp.recv(buf)
    });
    let (result, buf) = ready!(receiving.poll(cx));
    self.receiving = None;
    let processed = result.and_then(|n| {
      if n == 0 {
        return Err(io::Error::new(
          io::ErrorKind::UnexpectedEof,
          "connection closed",
        ));
      }
      let mut tls = &buf[..];
      while !tls.is_empty() {
        if self.stream.conn.read_tls(&mut tls)? == 0 {
          break;
        }
        self.stream.conn.process_new_packets().map_err(io::Error::other)?;
      }
      Ok(())
    });
    self.stream.recv_buf = buf;
    Step::Done(processed)
  }

  fn poll_handshake(&mut self, cx: &mut Cx<'_>) -> Step<io::Result<()>> {
    loop {
      tri!(ready!(self.poll_flush(cx)));
      if !self.stream.conn.is_handshaking() {
        return Step::Done(Ok(()));
      }
      // With nothing left to send, rustls stops reading only once the
      // peer has closed the session.
      if !self.stream.conn.wants_read() {
        return Step::Done(Err(io::Error::new(
          io::ErrorKind::UnexpectedEof,
          "peer closed the TLS session during the handshake",
        )));
      }
      tri!(ready!(self.poll_fill(cx)));
    }
  }

  /// Sends `data[*written..]`, counting what rustls has taken in
  /// `written`.
  fn poll_send(
    &mut self,
    cx: &mut Cx<'_>,
    data: &[u8],
    written: &mut usize,
  ) -> Step<io::Result<usize>> {
    loop {
      tri!(ready!(self.poll_flush(cx)));
      let Some(rest) = data.get(*written..).filter(|rest| !rest.is_empty())
      else {
        return Step::Done(Ok(*written));
      };
      // rustls takes as much as its send buffer allows, which after a
      // flush is more than nothing.
      match tri!(self.stream.conn.writer().write(rest)) {
        0 => {
          return Step::Done(Err(io::Error::new(
            io::ErrorKind::WriteZero,
            "rustls took no plaintext",
          )));
        }
        n => *written += n,
      }
    }
  }

  fn poll_recv(
    &mut self,
    cx: &mut Cx<'_>,
    buf: &mut [u8],
  ) -> Step<io::Result<usize>> {
    if buf.is_empty() {
      return Step::Done(Ok(0));
    }
    loop {
      if self.receiving.is_none() {
        match self.stream.conn.reader().read(buf) {
          Err(err) if err.kind() == io::ErrorKind::WouldBlock => {}
          done => return Step::Done(done),
        }
        // Whatever rustls has to say, a key update say, goes out first.
        tri!(ready!(self.poll_flush(cx)));
      }
      tri!(ready!(self.poll_fill(cx)));
    }
  }
}

#[cfg(test)]
mod tests {
  use super::{TlsAcceptor, TlsConnector};
  use rustls::pki_types::{CertificateDer, PrivateKeyDer};
  use std::sync::Arc;

  fn init_crypto() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
  }

  fn generate_test_certs()
  -> (Vec<CertificateDer<'static>>, PrivateKeyDer<'static>) {
    init_crypto();
    let cert =
      rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
        .unwrap();
    let key = PrivateKeyDer::Pkcs8(cert.key_pair.serialize_der().into());
    let cert = vec![CertificateDer::from(cert.cert.der().to_vec())];
    (cert, key)
  }

  #[test]
  fn connector_and_acceptor_can_be_constructed() {
    let (certs, key) = generate_test_certs();
    let root_cert = certs[0].clone();

    let server_cfg = rustls::ServerConfig::builder()
      .with_no_client_auth()
      .with_single_cert(certs, key)
      .unwrap();

    let mut roots = rustls::RootCertStore::empty();
    roots.add(root_cert).unwrap();
    let client_cfg = rustls::ClientConfig::builder()
      .with_root_certificates(roots)
      .with_no_client_auth();

    let _acceptor = TlsAcceptor::new(Arc::new(server_cfg));
    let _connector = TlsConnector::new(Arc::new(client_cfg));
  }
}
