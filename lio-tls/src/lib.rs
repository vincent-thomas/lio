//! TLS support for lio using rustls.
//!
//! This crate provides TLS client and server streams that integrate with
//! lio's async runtime.
//!
//! # Example (Client)
//!
//! ```no_run
//! use lio::net::TcpStream;
//! use lio_tls::TlsConnector;
//! use std::{net::SocketAddr, sync::Arc};
//!
//! async fn example() {
//!     let connector = TlsConnector::new(Arc::new(client_config()));
//!     let addr: SocketAddr = "93.184.216.34:443".parse().unwrap();
//!     let tcp = TcpStream::connect(addr).await.unwrap();
//!     let mut tls = connector.connect("example.com", tcp).await.unwrap();
//!
//!     tls.send(b"GET / HTTP/1.1\r\nHost: example.com\r\n\r\n".to_vec()).await.unwrap();
//!     let (buf, n) = tls.recv(vec![0u8; 4096]).await.unwrap();
//!     println!("{}", String::from_utf8_lossy(&buf[..n]));
//! }
//!
//! fn client_config() -> rustls::ClientConfig {
//!     // ... configure rustls
//!     todo!()
//! }
//! ```

use std::io::{self, Read, Write};
use std::sync::Arc;

use lio::net::TcpStream;
use rustls::{ClientConfig, ClientConnection, ServerConfig, ServerConnection};

/// A TLS connector for establishing client TLS connections.
pub struct TlsConnector {
  config: Arc<ClientConfig>,
}

impl TlsConnector {
  /// Creates a new TLS connector with the given configuration.
  pub fn new(config: Arc<ClientConfig>) -> Self {
    Self { config }
  }

  /// Establishes a TLS connection over the given TCP socket.
  pub async fn connect(
    &self,
    domain: &str,
    stream: TcpStream,
  ) -> io::Result<ClientTlsStream> {
    let server_name: rustls::pki_types::ServerName<'static> =
      domain.to_string().try_into().map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidInput, "invalid DNS name")
      })?;

    let conn = ClientConnection::new(self.config.clone(), server_name)
      .map_err(io::Error::other)?;

    let mut tls = ClientTlsStream { tcp: stream, conn };

    tls.handshake().await?;
    Ok(tls)
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

  /// Accepts a TLS connection over the given TCP socket.
  pub async fn accept(&self, stream: TcpStream) -> io::Result<ServerTlsStream> {
    let conn =
      ServerConnection::new(self.config.clone()).map_err(io::Error::other)?;

    let mut tls = ServerTlsStream { tcp: stream, conn };

    tls.handshake().await?;
    Ok(tls)
  }
}

/// A TLS client stream.
pub struct ClientTlsStream {
  tcp: TcpStream,
  conn: ClientConnection,
}

/// A TLS server stream.
pub struct ServerTlsStream {
  tcp: TcpStream,
  conn: ServerConnection,
}

macro_rules! impl_tls_stream {
  ($name:ident, $conn_ty:ty) => {
    impl $name {
      async fn handshake(&mut self) -> io::Result<()> {
        while self.conn.is_handshaking() {
          self.flush_tls().await?;

          if self.conn.is_handshaking() && self.conn.wants_read() {
            self.read_tls().await?;
          }
        }

        self.flush_tls().await?;
        Ok(())
      }

      async fn read_tls(&mut self) -> io::Result<()> {
        let buf = vec![0u8; 16384];
        let (result, buf) = self.tcp.recv(buf).await;
        let n = result? as usize;

        if n == 0 {
          return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "connection closed",
          ));
        }

        let mut slice = &buf[..n];
        while !slice.is_empty() {
          let read =
            self.conn.read_tls(&mut slice).map_err(io::Error::other)?;
          if read == 0 {
            break;
          }
          self.conn.process_new_packets().map_err(io::Error::other)?;
        }

        Ok(())
      }

      async fn flush_tls(&mut self) -> io::Result<()> {
        while self.conn.wants_write() {
          let mut buf = Vec::with_capacity(8192);
          self.conn.write_tls(&mut buf)?;

          let mut written = 0;
          while written < buf.len() {
            let chunk = buf[written..].to_vec();
            let (result, _chunk) = self.tcp.send(chunk).await;
            let n = result? as usize;
            if n == 0 {
              return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "failed to write TLS bytes",
              ));
            }
            written += n;
          }
        }
        Ok(())
      }

      /// Sends plaintext data over the TLS connection.
      ///
      /// Returns the buffer and the number of bytes sent.
      pub async fn send(
        &mut self,
        buf: Vec<u8>,
      ) -> io::Result<(Vec<u8>, usize)> {
        self.conn.writer().write_all(&buf)?;
        self.flush_tls().await?;
        let n = buf.len();
        Ok((buf, n))
      }

      /// Receives plaintext data from the TLS connection.
      ///
      /// Returns the buffer and the number of bytes received.
      pub async fn recv(
        &mut self,
        mut buf: Vec<u8>,
      ) -> io::Result<(Vec<u8>, usize)> {
        loop {
          match self.conn.reader().read(&mut buf) {
            Ok(n) if n > 0 => return Ok((buf, n)),
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(e),
            Ok(_) => {}
          }

          if self.conn.wants_read() {
            self.read_tls().await?;
          } else {
            return Ok((buf, 0));
          }
        }
      }

      /// Returns a reference to the underlying TCP socket.
      pub fn get_ref(&self) -> &TcpStream {
        &self.tcp
      }
    }
  };
}

impl_tls_stream!(ClientTlsStream, ClientConnection);
impl_tls_stream!(ServerTlsStream, ServerConnection);

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
