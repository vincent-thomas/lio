//! `wasi:cli@0.3.2`: the component's standard streams, arguments,
//! environment and exit.
//!
//! None of these functions wait, so they are WASI's own, passed on as they
//! are: [`stdin::read_via_stream`], [`stdout::write_via_stream`],
//! [`environment::get_arguments`], and so on. A standard stream is a byte
//! stream like any other: add the end this component keeps to a
//! [`WaitableSet`](crate::WaitableSet).
//!
//! What waits is a stream's status, the future those functions return:
//! wrap it in a [`StreamStatus`] to read it.

use core::cell::UnsafeCell;

pub use wasip3::cli::types::ErrorCode;
pub use wasip3::cli::{
  environment, exit, stderr, stdin, stdout, terminal_input, terminal_output,
  terminal_stderr, terminal_stdin, terminal_stdout,
};
use wasip3::wit_bindgen::FutureReader;
use wasip3::wit_bindgen::rt::async_support::FutureVtable;
use wasip3::wit_future::FuturePayload;

use crate::ReadableFuture;
use crate::reply::NO_RESULT;
use crate::sealed::{self, FutureStart};

/// `result<_, error-code>` as the host writes it: the result's tag, then
/// the error's case. This error code is a plain enum, so both are one byte.
type Reply = [u8; 2];

/// Lifts a status the host wrote. `None` while no status is there.
fn lift(reply: Reply) -> Option<Result<(), ErrorCode>> {
  match reply {
    [NO_RESULT, _] => None,
    [0, _] => Some(Ok(())),
    [1, 0] => Some(Err(ErrorCode::Io)),
    [1, 1] => Some(Err(ErrorCode::IllegalByteSequence)),
    [1, 2] => Some(Err(ErrorCode::Pipe)),
    [tag, case] => unreachable!("unknown status: tag {tag:#x}, case {case}"),
  }
}

/// How a standard stream ended: `Ok` at the end of the data, or the error
/// that cut it short.
///
/// [`stdin::read_via_stream`], [`stdout::write_via_stream`] and
/// [`stderr::write_via_stream`] return it as a future. Keep it for as long
/// as the stream is in use. Read it with
/// [`WaitableSet::read_future`](crate::WaitableSet::read_future) to tell an
/// error from the end; it is delivered once the stream has ended.
///
/// # Panics
///
/// Reading it again after it was delivered panics: a status is delivered
/// once.
pub struct StreamStatus {
  handle: u32,
  /// The value was read. A future delivers once; another read would trap.
  finished: bool,
  /// The host writes through a pointer into this.
  reply: UnsafeCell<Reply>,
  vtable: &'static FutureVtable<Result<(), ErrorCode>>,
}

impl StreamStatus {
  pub fn new(result: FutureReader<Result<(), ErrorCode>>) -> Self {
    Self {
      handle: result.take_handle(),
      finished: false,
      reply: UnsafeCell::new([NO_RESULT, 0]),
      vtable: <Result<(), ErrorCode> as FuturePayload>::VTABLE,
    }
  }

  /// Takes the status.
  ///
  /// Returns `None` if it has not arrived, was cancelled, or was already
  /// taken.
  pub fn result(&mut self) -> Option<Result<(), ErrorCode>> {
    let reply = core::mem::replace(self.reply.get_mut(), [NO_RESULT, 0]);
    let result = lift(reply);
    self.finished |= result.is_some();
    result
  }
}

impl ReadableFuture for StreamStatus {}

impl sealed::Future for StreamStatus {
  unsafe fn start_read(&mut self) -> FutureStart {
    if self.finished || self.reply.get_mut()[0] != NO_RESULT {
      panic!("a stream's status was read again after it was delivered");
    }
    // SAFETY: the handle is this future's readable end with no read in
    // flight, and the caller keeps `self`, which the host writes into, in
    // place until the read ends.
    let code =
      unsafe { (self.vtable.start_read)(self.handle, self.reply.get().cast()) };
    FutureStart { handle: self.handle, code, cancel: self.vtable.cancel_read }
  }
}

impl Drop for StreamStatus {
  fn drop(&mut self) {
    // SAFETY: the handle is owned, has no read in flight, and is dropped
    // once.
    unsafe { (self.vtable.drop_readable)(self.handle) }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn the_reply_matches_the_hosts_layout() {
    let layout = <Result<(), ErrorCode> as FuturePayload>::VTABLE.layout;
    assert_eq!((layout.size(), layout.align()), (2, 1));
  }

  #[test]
  fn no_status_lifts_to_nothing() {
    assert!(lift([NO_RESULT, 0]).is_none());
  }

  #[test]
  fn an_ok_status_ignores_the_case_byte() {
    assert!(matches!(lift([0, 7]), Some(Ok(()))));
  }

  #[test]
  fn error_cases_are_in_wit_order() {
    assert!(matches!(lift([1, 0]), Some(Err(ErrorCode::Io))));
    assert!(matches!(lift([1, 1]), Some(Err(ErrorCode::IllegalByteSequence))));
    assert!(matches!(lift([1, 2]), Some(Err(ErrorCode::Pipe))));
  }
}
