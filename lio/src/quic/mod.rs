//! QUIC support for lio using quinn-proto.
//!
//! This module provides QUIC client and server connections that integrate with
//! lio's async I/O.
//!
//! # Example (Client)
//!
//! ```no_run
//! use lio::quic::{Endpoint, ClientConfig};
//! use std::net::SocketAddr;
//!
//! async fn example() -> Result<(), lio::quic::Error> {
//!     let endpoint = Endpoint::client("0.0.0.0:0".parse().unwrap()).await?;
//!     let config = ClientConfig::insecure_skip_verify();
//!
//!     let conn = endpoint
//!         .connect("127.0.0.1:4433".parse().unwrap(), "localhost", config)
//!         .await?;
//!
//!     let mut stream = conn.open_bi().await?;
//!     stream.send.write_all(b"hello").await?;
//!     stream.send.finish().await?;
//!
//!     let mut buf = vec![0u8; 4096];
//!     let n = stream.recv.read(&mut buf).await?.unwrap_or(0);
//!     println!("{}", String::from_utf8_lossy(&buf[..n]));
//!     Ok(())
//! }
//! ```

mod connection;
mod endpoint;
mod error;
mod stream;
mod udp;

pub use endpoint::{Connection, Endpoint};
pub use error::Error;
pub use stream::{BiStream, RecvStream, SendStream};

use std::sync::Arc;

/// Client configuration for QUIC connections.
pub struct ClientConfig {
    pub(crate) inner: quinn_proto::ClientConfig,
}

impl ClientConfig {
    /// Creates a client configuration with the given root certificates.
    pub fn with_root_certificates(roots: rustls::RootCertStore) -> Result<Self, Error> {
        let crypto = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        Ok(Self {
            inner: quinn_proto::ClientConfig::new(Arc::new(
                quinn_proto::crypto::rustls::QuicClientConfig::try_from(crypto)
                    .map_err(|e| Error::Config(e.to_string()))?,
            )),
        })
    }

    /// Creates a client configuration that skips server certificate verification.
    ///
    /// # Safety
    ///
    /// This is insecure and should only be used for testing.
    pub fn insecure_skip_verify() -> Self {
        let crypto = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(SkipServerVerification))
            .with_no_client_auth();
        Self {
            inner: quinn_proto::ClientConfig::new(Arc::new(
                quinn_proto::crypto::rustls::QuicClientConfig::try_from(crypto).unwrap(),
            )),
        }
    }
}

/// Server configuration for QUIC connections.
pub struct ServerConfig {
    pub(crate) inner: quinn_proto::ServerConfig,
}

impl ServerConfig {
    /// Creates a server configuration with the given certificate chain and private key.
    pub fn with_single_cert(
        cert_chain: Vec<rustls::pki_types::CertificateDer<'static>>,
        key: rustls::pki_types::PrivateKeyDer<'static>,
    ) -> Result<Self, Error> {
        let crypto = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(cert_chain, key)
            .map_err(|e| Error::Config(e.to_string()))?;
        Ok(Self {
            inner: quinn_proto::ServerConfig::with_crypto(Arc::new(
                quinn_proto::crypto::rustls::QuicServerConfig::try_from(crypto)
                    .map_err(|e| Error::Config(e.to_string()))?,
            )),
        })
    }
}

#[derive(Debug)]
struct SkipServerVerification;

impl rustls::client::danger::ServerCertVerifier for SkipServerVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        vec![
            rustls::SignatureScheme::RSA_PKCS1_SHA256,
            rustls::SignatureScheme::RSA_PKCS1_SHA384,
            rustls::SignatureScheme::RSA_PKCS1_SHA512,
            rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
            rustls::SignatureScheme::ECDSA_NISTP384_SHA384,
            rustls::SignatureScheme::ECDSA_NISTP521_SHA512,
            rustls::SignatureScheme::RSA_PSS_SHA256,
            rustls::SignatureScheme::RSA_PSS_SHA384,
            rustls::SignatureScheme::RSA_PSS_SHA512,
            rustls::SignatureScheme::ED25519,
        ]
    }
}

