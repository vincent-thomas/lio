//! The component's standard streams, from `wasi:cli@0.3.2`.
//!
//! Each is a byte stream. Add the end this component keeps to a
//! [`WaitableSet`](crate::WaitableSet) and read or write it like any other.
//! These functions do not wait.

use wasip3::wit_bindgen::{FutureReader, StreamReader};

pub use wasip3::cli::types::ErrorCode;

/// How a standard stream ended.
///
/// Keep it for as long as the stream is in use. Reading it is not bound
/// yet: a stream that ends reports
/// [`StreamResult::Dropped`](crate::StreamResult::Dropped) either way.
pub struct StreamStatus {
  _future: FutureReader<Result<(), ErrorCode>>,
}

impl StreamStatus {
  fn new(future: FutureReader<Result<(), ErrorCode>>) -> Self {
    Self { _future: future }
  }
}

/// Standard input: the readable end of its stream, for
/// [`WaitableSet::add_reader`](crate::WaitableSet::add_reader).
pub fn stdin() -> (StreamReader<u8>, StreamStatus) {
  let (stream, status) = wasip3::cli::stdin::read_via_stream();
  (stream, StreamStatus::new(status))
}

/// Sends what is written to a stream to standard output.
///
/// `data` is the readable end of a stream from [`stream`](crate::stream),
/// whose writable end goes to
/// [`WaitableSet::add_writer`](crate::WaitableSet::add_writer).
pub fn stdout(data: StreamReader<u8>) -> StreamStatus {
  StreamStatus::new(wasip3::cli::stdout::write_via_stream(data))
}

/// Sends what is written to a stream to standard error, like
/// [`stdout`].
pub fn stderr(data: StreamReader<u8>) -> StreamStatus {
  StreamStatus::new(wasip3::cli::stderr::write_via_stream(data))
}
