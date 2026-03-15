//! Error types for lio-quic.

use std::io;

/// Errors that can occur in QUIC operations.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// An I/O error occurred.
    #[error("io error: {0}")]
    Io(#[from] io::Error),

    /// A configuration error occurred.
    #[error("config error: {0}")]
    Config(String),

    /// The connection was closed.
    #[error("connection closed: {0}")]
    ConnectionClosed(String),

    /// The stream was closed.
    #[error("stream closed")]
    StreamClosed,

    /// The connection timed out.
    #[error("connection timed out")]
    Timeout,

    /// The connection was refused.
    #[error("connection refused")]
    ConnectionRefused,

    /// Failed to connect.
    #[error("connect error: {0}")]
    Connect(String),

    /// A protocol error occurred.
    #[error("protocol error: {0}")]
    Protocol(String),

    /// Stream write error.
    #[error("write error: {0}")]
    Write(String),

    /// Stream read error.
    #[error("read error: {0}")]
    Read(String),
}

impl From<quinn_proto::ConnectionError> for Error {
    fn from(e: quinn_proto::ConnectionError) -> Self {
        match e {
            quinn_proto::ConnectionError::TimedOut => Error::Timeout,
            quinn_proto::ConnectionError::ConnectionClosed(frame) => {
                Error::ConnectionClosed(format!("{:?}", frame))
            }
            quinn_proto::ConnectionError::ApplicationClosed(frame) => {
                Error::ConnectionClosed(format!("{:?}", frame))
            }
            quinn_proto::ConnectionError::Reset => Error::ConnectionClosed("reset".into()),
            quinn_proto::ConnectionError::LocallyClosed => {
                Error::ConnectionClosed("locally closed".into())
            }
            e => Error::Protocol(e.to_string()),
        }
    }
}

impl From<quinn_proto::ConnectError> for Error {
    fn from(e: quinn_proto::ConnectError) -> Self {
        Error::Connect(e.to_string())
    }
}

impl From<quinn_proto::WriteError> for Error {
    fn from(e: quinn_proto::WriteError) -> Self {
        Error::Write(e.to_string())
    }
}

impl From<quinn_proto::ReadError> for Error {
    fn from(e: quinn_proto::ReadError) -> Self {
        Error::Read(e.to_string())
    }
}

impl From<quinn_proto::ReadableError> for Error {
    fn from(e: quinn_proto::ReadableError) -> Self {
        Error::Read(e.to_string())
    }
}

impl From<quinn_proto::AcceptError> for Error {
    fn from(e: quinn_proto::AcceptError) -> Self {
        Error::Connect(format!("{:?}", e))
    }
}

impl From<quinn_proto::FinishError> for Error {
    fn from(e: quinn_proto::FinishError) -> Self {
        Error::Write(e.to_string())
    }
}
