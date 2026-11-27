//! Shared by the examples: text output through lio-wasi itself.
//!
//! The examples use nothing but `lio_wasi` for I/O, so what they report goes
//! out over the component's standard streams the same way their data does.

#![allow(dead_code)]

use lio_wasi::cli::{self, StreamStatus};
use lio_wasi::{Event, Started, StreamResult, WaitableSet, WriteStream};

/// One of the component's output streams, in a set of its own so that
/// writing to it never meets an example's own events.
pub struct Out {
  set: WaitableSet,
  stream: WriteStream,
  _status: StreamStatus,
}

impl Out {
  /// Standard output.
  pub fn stdout() -> Self {
    let (writer, reader) = lio_wasi::stream();
    Self::over(writer, cli::stdout(reader))
  }

  /// Standard error.
  pub fn stderr() -> Self {
    let (writer, reader) = lio_wasi::stream();
    Self::over(writer, cli::stderr(reader))
  }

  fn over(writer: lio_wasi::StreamWriter<u8>, status: StreamStatus) -> Self {
    let mut set = WaitableSet::new();
    let stream = set.add_writer(writer);
    Self { set, stream, _status: status }
  }

  /// Writes `text` and a newline, waiting until the host has all of it.
  pub fn line(&mut self, text: &str) {
    self.all(text.as_bytes());
    self.all(b"\n");
  }

  fn all(&mut self, mut rest: &[u8]) {
    while !rest.is_empty() {
      // SAFETY: `rest` is borrowed for this whole function, and the write
      // has ended once this statement has.
      let started =
        unsafe { self.set.write(self.stream, rest.as_ptr(), rest.len()) };
      let wrote = match started {
        Ok(Started::Done(wrote)) => wrote,
        Ok(Started::Blocked) => match self.set.wait() {
          Some(Event::Write(stream, wrote)) if stream == self.stream => wrote,
          other => unreachable!("only the write is in flight: {other:?}"),
        },
        Err(error) => panic!("the output stream refused a write: {error}"),
      };
      let StreamResult::Completed(count) = wrote else {
        panic!("the output stream ended: {wrote:?}");
      };
      rest = &rest[count..];
    }
  }
}

/// Writes a formatted line to an [`Out`].
#[macro_export]
macro_rules! say {
  ($out:expr, $($arg:tt)*) => {
    $out.line(&format!($($arg)*))
  };
}
