//! Result storage the host writes into, and how results are lifted out.
//!
//! Most WASI calls return `result<T, error-code>` where both sides fit in
//! five pointer-sized words. [`Slot`] holds one such result, hands the host a
//! pointer to it, and gives the lifted value back exactly once.

use alloc::string::String;
use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::marker::PhantomData;

use wasip3::wit_bindgen::FutureReader;
use wasip3::wit_bindgen::rt::async_support::FutureVtable;

use crate::sealed::FutureStart;

/// Stored in a result's tag while no result is there: before the call
/// returns, after a cancel, or once the result was taken. The host only
/// writes `0` (ok) or `1` (error).
pub(crate) const NO_RESULT: u8 = 0xff;

/// The four words after a result's tag. What they mean depends on the case.
///
/// A one-byte field (an enum case, an option tag, flags) occupies the low
/// byte of its word; the host leaves the rest of the word alone.
#[derive(Clone, Copy)]
#[repr(C)]
pub(crate) struct Words {
  pub(crate) a: usize,
  pub(crate) b: usize,
  pub(crate) c: *mut u8,
  pub(crate) d: usize,
}

impl Words {
  pub(crate) const fn zeroed() -> Self {
    Self { a: 0, b: 0, c: core::ptr::null_mut(), d: 0 }
  }

  /// The one-byte field in the first word.
  pub(crate) fn case(&self) -> u8 {
    self.a.to_le_bytes()[0]
  }

  /// Adopts the `option<string>` in the last three words.
  ///
  /// # Safety
  ///
  /// The host wrote an `option<string>` there, and it is adopted once.
  pub(crate) unsafe fn message(&self) -> Option<String> {
    match self.b.to_le_bytes()[0] {
      0 => None,
      // SAFETY: the caller vouches for the string.
      1 => Some(unsafe { adopt_string(self.c, self.d) }),
      other => unreachable!("unknown option tag {other:#x}"),
    }
  }

  /// Lifts an error whose case is in the first word and whose optional
  /// message follows.
  ///
  /// # Safety
  ///
  /// The host wrote an `E` there, and it is lifted once.
  pub(crate) unsafe fn error<E: LiftError>(&self) -> E {
    match E::plain(self.case()) {
      Some(error) => error,
      // SAFETY: the only case with a payload carries the message.
      None => E::other(unsafe { self.message() }),
    }
  }
}

/// Adopts a string the host allocated in this component.
///
/// # Safety
///
/// For a non-zero `len`, the host allocated exactly `len` bytes with
/// alignment 1 through this component's allocator at `ptr` and handed them
/// over. They are adopted once.
pub(crate) unsafe fn adopt_string(ptr: *mut u8, len: usize) -> String {
  if len == 0 {
    return String::new();
  }
  // SAFETY: the caller vouches for the allocation.
  let bytes = unsafe { Vec::from_raw_parts(ptr, len, len) };
  match String::from_utf8(bytes) {
    Ok(text) => text,
    Err(invalid) => String::from_utf8_lossy(invalid.as_bytes()).into_owned(),
  }
}

/// An `error-code` variant: plain cases, then one case with a message.
pub(crate) trait LiftError: Sized {
  /// The plain case numbered `case`, or `None` for the case with a message.
  fn plain(case: u8) -> Option<Self>;
  /// The case with a message.
  fn other(message: Option<String>) -> Self;
}

/// The ok side of a result that fits in [`Words`].
pub(crate) trait LiftOk: Sized {
  /// # Safety
  ///
  /// The host wrote an ok payload of this type, and it is lifted once.
  unsafe fn lift(words: &Words) -> Self;
}

impl LiftOk for () {
  unsafe fn lift(_words: &Words) -> Self {}
}

impl LiftOk for String {
  unsafe fn lift(words: &Words) -> Self {
    // SAFETY: a string is its pointer then its length.
    unsafe { adopt_string(words.a as *mut u8, words.b) }
  }
}

/// `result<T, E>` as the host writes it.
#[repr(C)]
struct Reply {
  /// `0` for ok, `1` for error, [`NO_RESULT`] otherwise.
  tag: u8,
  words: Words,
}

#[cfg(target_pointer_width = "32")]
const _: () = {
  assert!(size_of::<Reply>() == 20);
  assert!(align_of::<Reply>() == 4);
  assert!(core::mem::offset_of!(Reply, words) == 4);
  assert!(core::mem::offset_of!(Words, b) == 4);
  assert!(core::mem::offset_of!(Words, c) == 8);
  assert!(core::mem::offset_of!(Words, d) == 12);
};

/// Storage for one `result<T, E>`.
pub(crate) struct Slot<T: LiftOk, E: LiftError> {
  /// The host writes through a pointer into this.
  reply: UnsafeCell<Reply>,
  marker: PhantomData<(T, E)>,
}

impl<T: LiftOk, E: LiftError> Slot<T, E> {
  pub(crate) const fn new() -> Self {
    Self {
      reply: UnsafeCell::new(Reply { tag: NO_RESULT, words: Words::zeroed() }),
      marker: PhantomData,
    }
  }

  /// Releases any result still here and returns where the host writes the
  /// next one.
  pub(crate) fn arm(&mut self) -> *mut u8 {
    drop(self.take());
    self.reply.get().cast()
  }

  /// Takes the result out, leaving [`NO_RESULT`] behind.
  pub(crate) fn take(&mut self) -> Option<Result<T, E>> {
    let reply = self.reply.get_mut();
    match core::mem::replace(&mut reply.tag, NO_RESULT) {
      NO_RESULT => None,
      // SAFETY: the host wrote an ok payload, and the tag was cleared, so
      // it is lifted once.
      0 => Some(Ok(unsafe { T::lift(&reply.words) })),
      // SAFETY: as above, for the error.
      1 => Some(Err(unsafe { reply.words.error() })),
      other => unreachable!("unknown result tag {other:#x}"),
    }
  }

  /// Whether a result is here and has not been taken.
  pub(crate) fn is_filled(&mut self) -> bool {
    self.reply.get_mut().tag != NO_RESULT
  }

  /// Writes a result as the host would. Tests only.
  #[cfg(test)]
  pub(crate) fn set(&mut self, tag: u8, words: Words) {
    *self.reply.get_mut() = Reply { tag, words };
  }
}

impl<T: LiftOk, E: LiftError> Drop for Slot<T, E> {
  fn drop(&mut self) {
    // An untaken result may own a handle or a string. Release it.
    drop(self.take());
  }
}

/// The `future<result<_, E>>` that reports how a stream ended.
///
/// Owns the future's readable end and the storage its value is read into.
pub(crate) struct ResultFuture<E: LiftError + 'static> {
  handle: u32,
  /// The value was read. A future delivers once; another read would trap.
  finished: bool,
  reply: Slot<(), E>,
  vtable: &'static FutureVtable<Result<(), E>>,
}

impl<E: LiftError + 'static> ResultFuture<E> {
  pub(crate) fn new(
    reader: FutureReader<Result<(), E>>,
    vtable: &'static FutureVtable<Result<(), E>>,
  ) -> Self {
    Self {
      handle: reader.take_handle(),
      finished: false,
      reply: Slot::new(),
      vtable,
    }
  }

  /// Starts reading the value.
  ///
  /// # Panics
  ///
  /// If the value was already read.
  ///
  /// # Safety
  ///
  /// No read is in flight. `self` stays allocated, unmoved and untouched
  /// until the read completes or is cancelled.
  pub(crate) unsafe fn start(&mut self) -> FutureStart {
    if self.finished || self.reply.is_filled() {
      panic!("a stream's status was read again after it was delivered");
    }
    let reply = self.reply.arm();
    // SAFETY: the handle is this future's readable end with no read in
    // flight, and the caller keeps `reply` in place.
    let code = unsafe { (self.vtable.start_read)(self.handle, reply) };
    FutureStart { handle: self.handle, code, cancel: self.vtable.cancel_read }
  }

  pub(crate) fn take(&mut self) -> Option<Result<(), E>> {
    let result = self.reply.take();
    self.finished |= result.is_some();
    result
  }
}

impl<E: LiftError + 'static> Drop for ResultFuture<E> {
  fn drop(&mut self) {
    // SAFETY: the handle is owned, has no read in flight, and is dropped
    // once.
    unsafe { (self.vtable.drop_readable)(self.handle) }
  }
}

/// Gives an operation its `result` method and makes it a [`Call`].
///
/// It expands where `ErrorCode`, `Call` and `sealed` name the calling
/// module's items.
///
/// `$start` evaluates to the import's return value. It runs with `$op`
/// borrowing the operation and `$reply` pointing at its result storage.
macro_rules! call {
  ($name:ident => $out:ty, |$op:ident, $reply:ident| $start:expr) => {
    impl $name<'_> {
      /// Takes the call's result.
      ///
      /// Returns `None` if the call has not returned, was cancelled, or its
      /// result was already taken.
      pub fn result(&mut self) -> Option<Result<$out, ErrorCode>> {
        self.reply.take()
      }
    }

    impl Call for $name<'_> {}

    impl sealed::Call for $name<'_> {
      unsafe fn start(&mut self) -> u32 {
        let $reply = self.reply.arm();
        let $op = &*self;
        // SAFETY: the arguments are valid for the import, and the caller
        // keeps `self` and what it borrows in place until the call returns.
        (unsafe { $start }) as u32
      }
    }
  };
}

pub(crate) use call;

#[cfg(test)]
mod tests {
  use alloc::boxed::Box;

  use super::*;

  #[derive(Debug, PartialEq)]
  enum Fault {
    First,
    Second,
    Other(Option<String>),
  }

  impl LiftError for Fault {
    fn plain(case: u8) -> Option<Self> {
      match case {
        0 => Some(Fault::First),
        1 => Some(Fault::Second),
        2 => None,
        other => unreachable!("unknown case {other}"),
      }
    }
    fn other(message: Option<String>) -> Self {
      Fault::Other(message)
    }
  }

  fn words(a: usize) -> Words {
    Words { a, ..Words::zeroed() }
  }

  /// Hands over a string the way the host does: an exact allocation.
  fn host_string(text: &str) -> (*mut u8, usize) {
    let boxed = String::from(text).into_boxed_str();
    let len = boxed.len();
    (Box::into_raw(boxed).cast(), len)
  }

  #[test]
  fn a_new_slot_has_nothing_to_take() {
    assert_eq!(Slot::<(), Fault>::new().take(), None);
  }

  #[test]
  fn an_ok_result_is_taken_once() {
    let mut slot = Slot::<(), Fault>::new();
    slot.set(0, Words::zeroed());
    assert_eq!(slot.take(), Some(Ok(())));
    assert_eq!(slot.take(), None);
  }

  #[test]
  fn an_error_is_lifted_by_its_case() {
    let mut slot = Slot::<(), Fault>::new();
    slot.set(1, words(1));
    assert_eq!(slot.take(), Some(Err(Fault::Second)));
  }

  #[test]
  fn only_the_low_byte_of_a_case_word_counts() {
    let mut slot = Slot::<(), Fault>::new();
    slot.set(1, words(0xabcd_ef00));
    assert_eq!(slot.take(), Some(Err(Fault::First)));
  }

  #[test]
  fn the_message_case_without_a_message_is_none() {
    let mut slot = Slot::<(), Fault>::new();
    slot.set(1, words(2));
    assert_eq!(slot.take(), Some(Err(Fault::Other(None))));
  }

  #[test]
  fn the_message_case_adopts_the_hosts_string() {
    let (c, d) = host_string("disk on fire");
    let mut slot = Slot::<(), Fault>::new();
    slot.set(1, Words { a: 2, b: 1, c, d });
    assert_eq!(
      slot.take(),
      Some(Err(Fault::Other(Some("disk on fire".into()))))
    );
  }

  #[test]
  fn an_empty_message_reads_no_pointer() {
    let mut slot = Slot::<(), Fault>::new();
    slot.set(1, Words { a: 2, b: 1, ..Words::zeroed() });
    assert_eq!(slot.take(), Some(Err(Fault::Other(Some(String::new())))));
  }

  #[test]
  fn an_ok_string_is_adopted() {
    let (ptr, len) = host_string("target");
    let mut slot = Slot::<String, Fault>::new();
    slot.set(0, Words { a: ptr as usize, b: len, ..Words::zeroed() });
    assert_eq!(slot.take(), Some(Ok("target".into())));
  }

  #[test]
  fn arming_releases_an_untaken_result() {
    let (c, d) = host_string("left behind");
    let mut slot = Slot::<(), Fault>::new();
    slot.set(1, Words { a: 2, b: 1, c, d });
    slot.arm();
    assert_eq!(slot.take(), None);
  }

  #[test]
  fn dropping_releases_an_untaken_result() {
    let (c, d) = host_string("left behind");
    let mut slot = Slot::<(), Fault>::new();
    slot.set(1, Words { a: 2, b: 1, c, d });
    drop(slot);
  }
}
