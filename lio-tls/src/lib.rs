//! TLS support for lio using rustls.
//!
//! This crate provides TLS client and server streams that integrate with
//! lio's async I/O.
//!
//! # Example (Client)
//!
//! ```no_run
//! use lio::net::TcpSocket;
//! use lio_tls::TlsConnector;
//! use std::sync::Arc;
//!
//! async fn example() {
//!     let connector = TlsConnector::new(Arc::new(client_config()));
//!     let tcp = TcpSocket::connect_async("127.0.0.1:443".parse().unwrap()).await.unwrap();
//!     let mut tls = connector.connect("localhost", tcp).await.unwrap();
//!
//!     tls.send(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n".to_vec()).await.unwrap();
//!     let (buf, n) = tls.recv(vec![0u8; 4096]).await.unwrap();
//!     println!("{}", String::from_utf8_lossy(&buf[..n]));
//! }
//!
//! fn client_config() -> rustls::ClientConfig {
//!     todo!()
//! }
//! ```

use std::io::{self, Read, Write};
use std::sync::Arc;

use lio::net::TcpSocket;
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
        socket: TcpSocket,
    ) -> io::Result<ClientTlsStream> {
        let server_name: rustls::pki_types::ServerName<'static> = domain
            .to_string()
            .try_into()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid DNS name"))?;

        let conn = ClientConnection::new(self.config.clone(), server_name)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

        let mut tls = ClientTlsStream { tcp: socket, conn };

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
    pub async fn accept(&self, socket: TcpSocket) -> io::Result<ServerTlsStream> {
        let conn = ServerConnection::new(self.config.clone())
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

        let mut tls = ServerTlsStream { tcp: socket, conn };

        tls.handshake().await?;
        Ok(tls)
    }
}

/// A TLS client stream.
pub struct ClientTlsStream {
    tcp: TcpSocket,
    conn: ClientConnection,
}

/// A TLS server stream.
pub struct ServerTlsStream {
    tcp: TcpSocket,
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

                self.conn
                    .read_tls(&mut &buf[..n])
                    .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

                self.conn
                    .process_new_packets()
                    .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

                Ok(())
            }

            async fn flush_tls(&mut self) -> io::Result<()> {
                while self.conn.wants_write() {
                    let mut buf = Vec::with_capacity(8192);
                    self.conn.write_tls(&mut buf)?;

                    if !buf.is_empty() {
                        let (result, _) = self.tcp.send(buf).await;
                        result?;
                    }
                }
                Ok(())
            }

            /// Sends plaintext data over the TLS connection.
            ///
            /// Returns the buffer and the number of bytes sent.
            pub async fn send(&mut self, buf: Vec<u8>) -> io::Result<(Vec<u8>, usize)> {
                let n = self.conn.writer().write(&buf)?;
                self.flush_tls().await?;
                Ok((buf, n))
            }

            /// Receives plaintext data from the TLS connection.
            ///
            /// Returns the buffer and the number of bytes received.
            pub async fn recv(&mut self, mut buf: Vec<u8>) -> io::Result<(Vec<u8>, usize)> {
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
            pub fn get_ref(&self) -> &TcpSocket {
                &self.tcp
            }
        }
    };
}

impl_tls_stream!(ClientTlsStream, ClientConnection);
impl_tls_stream!(ServerTlsStream, ServerConnection);
