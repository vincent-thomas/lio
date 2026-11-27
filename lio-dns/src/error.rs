use std::{fmt, io};

use hickory_proto::{ProtoError, op::ResponseCode};

/// Why a lookup has no answer.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
  /// The name is not a valid domain name.
  InvalidName(ProtoError),
  /// The query could not be encoded.
  Encode(ProtoError),
  /// The configuration lists no nameserver.
  NoNameservers,
  /// Every name tried exists without records of the asked type, or does
  /// not exist. `code` is the last answer's: `NXDomain` when the name does
  /// not exist, `NoError` when it has no such records.
  NoRecords {
    /// The name as asked for.
    name: String,
    /// The response code of the last answer.
    code: ResponseCode,
  },
  /// A nameserver answered with an error, such as `ServFail` or `Refused`,
  /// and none answered otherwise.
  Server(ResponseCode),
  /// No nameserver answered in time.
  Timeout,
  /// Talking to a nameserver failed, and none answered otherwise.
  Io(io::Error),
}

impl fmt::Display for Error {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::InvalidName(err) => write!(f, "invalid domain name: {err}"),
      Self::Encode(err) => write!(f, "could not encode the query: {err}"),
      Self::NoNameservers => f.write_str("no nameserver is configured"),
      Self::NoRecords { name, code } => {
        write!(f, "no records found for {name} ({code})")
      }
      Self::Server(code) => write!(f, "the nameservers answered {code}"),
      Self::Timeout => f.write_str("no nameserver answered in time"),
      Self::Io(err) => write!(f, "could not reach a nameserver: {err}"),
    }
  }
}

impl std::error::Error for Error {
  fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
    match self {
      Self::InvalidName(err) | Self::Encode(err) => Some(err),
      Self::Io(err) => Some(err),
      _ => None,
    }
  }
}

impl From<Error> for io::Error {
  fn from(err: Error) -> Self {
    let kind = match &err {
      Error::InvalidName(_) => io::ErrorKind::InvalidInput,
      Error::NoRecords { .. } => io::ErrorKind::NotFound,
      Error::Timeout => io::ErrorKind::TimedOut,
      Error::Io(err) => err.kind(),
      _ => io::ErrorKind::Other,
    };
    io::Error::new(kind, err)
  }
}
