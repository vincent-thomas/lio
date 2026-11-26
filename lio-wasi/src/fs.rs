//! Calls on `wasi:filesystem/types@0.3.2`.
//!
//! Each operation value holds the call's arguments and result in the
//! canonical-ABI layout the host reads and writes. Nothing is allocated to
//! make a call: a path is passed by pointer, and the result lands in the
//! operation value.
//!
//! The layouts here are written by hand to match what `wit-bindgen`
//! generates for the same functions. The assertions next to each one pin
//! them for `wasm32`.

use core::cell::UnsafeCell;
use core::marker::PhantomData;

pub use wasip3::filesystem::types::{
  Descriptor, DescriptorFlags, ErrorCode, OpenFlags, PathFlags,
};

use crate::{Call, sealed};

/// Stored in a result's tag while the call has not returned, or after the
/// result was taken. The host only ever writes `0` (ok) or `1` (error).
const NO_RESULT: u8 = 0xff;

/// `result<own<descriptor>, error-code>` as the host writes it.
#[repr(C)]
struct DescriptorResult {
  /// `0` for ok, `1` for error, [`NO_RESULT`] before the call returns.
  tag: u8,
  /// Ok: the new descriptor's handle. Error: the `error-code` case, in the
  /// first byte. A `u32` so the field sits at offset 4 as the ABI has it.
  word: u32,
  /// For `error-code::other`: whether a message follows.
  other_tag: u8,
  /// The message, allocated by the host through this component's allocator.
  other_ptr: *mut u8,
  other_len: usize,
}

#[cfg(target_pointer_width = "32")]
const _: () = {
  assert!(size_of::<DescriptorResult>() == 20);
  assert!(core::mem::offset_of!(DescriptorResult, word) == 4);
  assert!(core::mem::offset_of!(DescriptorResult, other_tag) == 8);
  assert!(core::mem::offset_of!(DescriptorResult, other_ptr) == 12);
  assert!(core::mem::offset_of!(DescriptorResult, other_len) == 16);
};

impl DescriptorResult {
  const fn empty() -> Self {
    Self {
      tag: NO_RESULT,
      word: 0,
      other_tag: 0,
      other_ptr: core::ptr::null_mut(),
      other_len: 0,
    }
  }

  /// Takes the result out, leaving [`NO_RESULT`] behind.
  fn take(&mut self) -> Option<Result<Descriptor, ErrorCode>> {
    match core::mem::replace(&mut self.tag, NO_RESULT) {
      NO_RESULT => None,
      0 => {
        let handle = self.word;
        // SAFETY: the host returned this handle as an owned descriptor, and
        // the tag was cleared, so it is adopted once.
        Some(Ok(unsafe { Descriptor::from_handle(handle) }))
      }
      1 => Some(Err(self.take_error())),
      other => unreachable!("unknown result tag {other:#x}"),
    }
  }

  /// Lifts the `error-code` of an error result.
  fn take_error(&mut self) -> ErrorCode {
    match self.word.to_le_bytes()[0] {
      0 => ErrorCode::Access,
      1 => ErrorCode::Already,
      2 => ErrorCode::BadDescriptor,
      3 => ErrorCode::Busy,
      4 => ErrorCode::Deadlock,
      5 => ErrorCode::Quota,
      6 => ErrorCode::Exist,
      7 => ErrorCode::FileTooLarge,
      8 => ErrorCode::IllegalByteSequence,
      9 => ErrorCode::InProgress,
      10 => ErrorCode::Interrupted,
      11 => ErrorCode::Invalid,
      12 => ErrorCode::Io,
      13 => ErrorCode::IsDirectory,
      14 => ErrorCode::Loop,
      15 => ErrorCode::TooManyLinks,
      16 => ErrorCode::MessageSize,
      17 => ErrorCode::NameTooLong,
      18 => ErrorCode::NoDevice,
      19 => ErrorCode::NoEntry,
      20 => ErrorCode::NoLock,
      21 => ErrorCode::InsufficientMemory,
      22 => ErrorCode::InsufficientSpace,
      23 => ErrorCode::NotDirectory,
      24 => ErrorCode::NotEmpty,
      25 => ErrorCode::NotRecoverable,
      26 => ErrorCode::Unsupported,
      27 => ErrorCode::NoTty,
      28 => ErrorCode::NoSuchDevice,
      29 => ErrorCode::Overflow,
      30 => ErrorCode::NotPermitted,
      31 => ErrorCode::Pipe,
      32 => ErrorCode::ReadOnly,
      33 => ErrorCode::InvalidSeek,
      34 => ErrorCode::TextFileBusy,
      35 => ErrorCode::CrossDevice,
      36 => ErrorCode::Other(self.take_message()),
      other => unreachable!("unknown error-code case {other}"),
    }
  }

  /// Takes the message of `error-code::other`, if the host sent one.
  fn take_message(&mut self) -> Option<String> {
    match self.other_tag {
      0 => None,
      1 if self.other_len == 0 => Some(String::new()),
      1 => {
        // SAFETY: the host allocated exactly `other_len` bytes with
        // alignment 1 through this component's allocator and handed them
        // over. The result tag was cleared, so they are adopted once.
        let bytes = unsafe {
          Vec::from_raw_parts(self.other_ptr, self.other_len, self.other_len)
        };
        Some(match String::from_utf8(bytes) {
          Ok(message) => message,
          Err(invalid) => {
            String::from_utf8_lossy(invalid.as_bytes()).into_owned()
          }
        })
      }
      other => unreachable!("unknown option tag {other:#x}"),
    }
  }
}

/// Arguments and result of `descriptor.open-at`, in one block: the import
/// takes a pointer to the arguments and a pointer to the result.
#[repr(C)]
struct OpenAtAbi {
  dir: u32,
  path_flags: u8,
  path_ptr: *const u8,
  path_len: usize,
  open_flags: u8,
  flags: u8,
  result: DescriptorResult,
}

#[cfg(target_pointer_width = "32")]
const _: () = {
  assert!(size_of::<OpenAtAbi>() == 40);
  assert!(align_of::<OpenAtAbi>() == 4);
  assert!(core::mem::offset_of!(OpenAtAbi, path_flags) == 4);
  assert!(core::mem::offset_of!(OpenAtAbi, path_ptr) == 8);
  assert!(core::mem::offset_of!(OpenAtAbi, path_len) == 12);
  assert!(core::mem::offset_of!(OpenAtAbi, open_flags) == 16);
  assert!(core::mem::offset_of!(OpenAtAbi, flags) == 17);
  assert!(core::mem::offset_of!(OpenAtAbi, result) == 20);
};

/// Opens a path relative to a directory, like `openat`.
///
/// Push it with [`Queue::push_call`](crate::Queue::push_call). After its
/// completion reports [`Outcome::Returned`](crate::Outcome::Returned), take
/// the new descriptor or the error with [`result`](Self::result).
///
/// ```rust,ignore
/// let mut open = OpenAt::new(
///   &dir,
///   PathFlags::empty(),
///   "data.txt",
///   OpenFlags::empty(),
///   DescriptorFlags::READ,
/// );
/// // SAFETY: `open` stays put until its completion is taken below.
/// unsafe { queue.push_call(&mut open, 7) };
/// queue.wait();
/// let file = open.result();
/// ```
pub struct OpenAt<'a> {
  /// The host writes the result through a pointer into this.
  abi: UnsafeCell<OpenAtAbi>,
  /// The directory and the path are read by the host while in flight.
  borrows: PhantomData<(&'a Descriptor, &'a str)>,
}

impl<'a> OpenAt<'a> {
  /// Describes opening `path` relative to `dir`.
  pub fn new(
    dir: &'a Descriptor,
    path_flags: PathFlags,
    path: &'a str,
    open_flags: OpenFlags,
    flags: DescriptorFlags,
  ) -> Self {
    Self {
      abi: UnsafeCell::new(OpenAtAbi {
        dir: dir.handle(),
        path_flags: path_flags.bits(),
        path_ptr: path.as_ptr(),
        path_len: path.len(),
        open_flags: open_flags.bits(),
        flags: flags.bits(),
        result: DescriptorResult::empty(),
      }),
      borrows: PhantomData,
    }
  }

  /// Takes the call's result.
  ///
  /// Returns `None` if the call has not returned, was cancelled, or its
  /// result was already taken.
  pub fn result(&mut self) -> Option<Result<Descriptor, ErrorCode>> {
    self.abi.get_mut().result.take()
  }
}

impl Drop for OpenAt<'_> {
  fn drop(&mut self) {
    // An untaken result owns a descriptor or a message. Release it.
    drop(self.result());
  }
}

impl Call for OpenAt<'_> {}

impl sealed::Sealed for OpenAt<'_> {
  unsafe fn start(&mut self) -> u32 {
    // A result left from an earlier use is released before it is
    // overwritten.
    drop(self.result());
    let abi = self.abi.get();
    // SAFETY: `abi` points into `self`. The caller keeps `self` in place
    // until the call returns, and the path and directory it names outlive
    // that.
    unsafe {
      let results = &raw mut (*abi).result;
      imports::open_at(abi.cast(), results.cast()) as u32
    }
  }
}

#[cfg(target_arch = "wasm32")]
mod imports {
  #[link(wasm_import_module = "wasi:filesystem/types@0.3.2")]
  unsafe extern "C" {
    #[link_name = "[async-lower][method]descriptor.open-at"]
    pub(super) fn open_at(params: *mut u8, results: *mut u8) -> i32;
  }
}

/// See `sys::imports`: there is no host to call off wasm.
#[cfg(not(target_arch = "wasm32"))]
mod imports {
  pub(super) unsafe fn open_at(_params: *mut u8, _results: *mut u8) -> i32 {
    panic!("lio-wasi runs only inside a WebAssembly component")
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn error(case: u8) -> DescriptorResult {
    let mut result = DescriptorResult::empty();
    result.tag = 1;
    result.word = u32::from(case);
    result
  }

  #[test]
  fn an_empty_result_has_nothing_to_take() {
    assert!(DescriptorResult::empty().take().is_none());
  }

  #[test]
  fn an_error_is_lifted_by_case_and_taken_once() {
    let mut result = error(19);
    assert!(matches!(result.take(), Some(Err(ErrorCode::NoEntry))));
    assert!(result.take().is_none());
  }

  #[test]
  fn the_first_and_last_plain_cases_are_in_order() {
    assert!(matches!(error(0).take(), Some(Err(ErrorCode::Access))));
    assert!(matches!(error(35).take(), Some(Err(ErrorCode::CrossDevice))));
  }

  #[test]
  fn other_without_a_message_is_none() {
    let mut result = error(36);
    assert!(matches!(result.take(), Some(Err(ErrorCode::Other(None)))));
  }

  #[test]
  fn other_adopts_the_message_the_host_allocated() {
    let message = String::from("disk on fire").into_boxed_str();
    let mut result = error(36);
    result.other_tag = 1;
    result.other_len = message.len();
    result.other_ptr = Box::into_raw(message).cast();
    let Some(Err(ErrorCode::Other(Some(text)))) = result.take() else {
      panic!("expected the message");
    };
    assert_eq!(text, "disk on fire");
  }

  #[test]
  fn other_with_an_empty_message_reads_no_pointer() {
    let mut result = error(36);
    result.other_tag = 1;
    let Some(Err(ErrorCode::Other(Some(text)))) = result.take() else {
      panic!("expected an empty message");
    };
    assert!(text.is_empty());
  }
}
