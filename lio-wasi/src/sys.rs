//! Raw component-model built-ins and the codes they return.
//!
//! These are the canonical-ABI imports that `wit-bindgen`'s futures are
//! built on. The waitable-set functions have fixed names in the `$root`
//! module. The stream functions are per payload type, so they come from the
//! vtable `wasip3` generates for `stream<u8>`.

use core::num::NonZeroU32;

use wasip3::wit_bindgen::rt::async_support::StreamVtable;
use wasip3::wit_stream::StreamPayload;

/// A stream call could not finish at once. Its result arrives as an event.
pub(crate) const BLOCKED: u32 = 0xffff_ffff;

/// Low four bits of a stream result: the operation transferred its items.
pub(crate) const COMPLETED: u32 = 0x0;
/// Low four bits of a stream result: the other end is gone.
pub(crate) const DROPPED: u32 = 0x1;
/// Low four bits of a stream result: the operation was cancelled.
pub(crate) const CANCELLED: u32 = 0x2;

/// `waitable-set.poll` found nothing ready.
pub(crate) const EVENT_NONE: u32 = 0;
/// A call to an async import changed state.
pub(crate) const EVENT_SUBTASK: u32 = 1;
/// A blocked `stream.read` finished.
pub(crate) const EVENT_STREAM_READ: u32 = 2;
/// A blocked `stream.write` finished.
pub(crate) const EVENT_STREAM_WRITE: u32 = 3;
/// A blocked `future.read` finished.
pub(crate) const EVENT_FUTURE_READ: u32 = 4;

/// A call's state: the callee has not taken the arguments yet.
pub(crate) const STATUS_STARTING: u32 = 0;
/// A call's state: the callee has the arguments and is running.
pub(crate) const STATUS_STARTED: u32 = 1;
/// A call's state: the results are written.
pub(crate) const STATUS_RETURNED: u32 = 2;
/// A call's state: cancelled before the callee took the arguments.
pub(crate) const STATUS_STARTED_CANCELLED: u32 = 3;
/// A call's state: cancelled before the callee wrote results.
pub(crate) const STATUS_RETURNED_CANCELLED: u32 = 4;

/// The canonical ABI caps one stream read or write at this many items.
pub(crate) const MAX_LENGTH: usize = (1 << 28) - 1;

/// The intrinsics for `stream<u8>`.
pub(crate) fn byte_stream() -> &'static StreamVtable<u8> {
  <u8 as StreamPayload>::VTABLE
}

#[cfg(target_arch = "wasm32")]
mod imports {
  #[link(wasm_import_module = "$root")]
  unsafe extern "C" {
    #[link_name = "[waitable-set-new]"]
    pub(super) fn set_new() -> u32;
    #[link_name = "[waitable-set-drop]"]
    pub(super) fn set_drop(set: u32);
    #[link_name = "[waitable-join]"]
    pub(super) fn join(waitable: u32, set: u32);
    #[link_name = "[waitable-set-wait]"]
    pub(super) fn set_wait(set: u32, payload: *mut [u32; 2]) -> u32;
    #[link_name = "[waitable-set-poll]"]
    pub(super) fn set_poll(set: u32, payload: *mut [u32; 2]) -> u32;
    #[link_name = "[subtask-cancel]"]
    pub(super) fn subtask_cancel(subtask: u32) -> u32;
    #[link_name = "[subtask-drop]"]
    pub(super) fn subtask_drop(subtask: u32);
  }
}

/// Off wasm there is no component-model host to call. The crate still
/// type-checks and its pure logic is testable; reaching an import is misuse.
#[cfg(not(target_arch = "wasm32"))]
mod imports {
  const NO_HOST: &str = "lio-wasi runs only inside a WebAssembly component";

  pub(super) unsafe fn set_new() -> u32 {
    panic!("{NO_HOST}")
  }
  pub(super) unsafe fn set_drop(_set: u32) {
    panic!("{NO_HOST}")
  }
  pub(super) unsafe fn join(_waitable: u32, _set: u32) {
    panic!("{NO_HOST}")
  }
  pub(super) unsafe fn set_wait(_set: u32, _payload: *mut [u32; 2]) -> u32 {
    panic!("{NO_HOST}")
  }
  pub(super) unsafe fn set_poll(_set: u32, _payload: *mut [u32; 2]) -> u32 {
    panic!("{NO_HOST}")
  }
  pub(super) unsafe fn subtask_cancel(_subtask: u32) -> u32 {
    panic!("{NO_HOST}")
  }
  pub(super) unsafe fn subtask_drop(_subtask: u32) {
    panic!("{NO_HOST}")
  }
}

/// Cancels a call that has not returned and waits for it to stop.
///
/// Returns the state it stopped in. The subtask must not be in a waitable
/// set.
pub(crate) fn subtask_cancel(subtask: u32) -> u32 {
  // SAFETY: a handle; a bad one traps instead of corrupting memory.
  unsafe { imports::subtask_cancel(subtask) }
}

/// Drops the handle of a call that has returned or been cancelled.
pub(crate) fn subtask_drop(subtask: u32) {
  // SAFETY: a handle; a bad one traps instead of corrupting memory.
  unsafe { imports::subtask_drop(subtask) }
}

/// One event from a waitable set: its kind, the waitable it is about, and
/// that waitable's result code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RawEvent {
  pub(crate) kind: u32,
  pub(crate) waitable: u32,
  pub(crate) code: u32,
}

/// An owned `waitable-set`.
///
/// Dropping a set that still has waitables joined traps, so the owner must
/// [`leave`](Self::leave) each one first.
pub(crate) struct RawSet(NonZeroU32);

impl RawSet {
  pub(crate) fn new() -> Self {
    // SAFETY: `waitable-set.new` takes no arguments and returns a new handle.
    let raw = unsafe { imports::set_new() };
    let Some(handle) = NonZeroU32::new(raw) else {
      unreachable!("waitable-set.new returned the null handle")
    };
    Self(handle)
  }

  /// Adds `waitable` to this set, moving it out of any other set.
  pub(crate) fn join(&self, waitable: u32) {
    // SAFETY: both are handles; a bad one traps instead of corrupting memory.
    unsafe { imports::join(waitable, self.0.get()) }
  }

  /// Removes `waitable` from whichever set it is in.
  pub(crate) fn leave(waitable: u32) {
    // SAFETY: set `0` means "no set"; a bad waitable traps.
    unsafe { imports::join(waitable, 0) }
  }

  /// Blocks until a joined waitable has an event.
  pub(crate) fn wait(&self) -> RawEvent {
    let mut payload = [0u32; 2];
    // SAFETY: `payload` is valid for the two words the host writes.
    let kind = unsafe { imports::set_wait(self.0.get(), &mut payload) };
    RawEvent { kind, waitable: payload[0], code: payload[1] }
  }

  /// Returns an event if one is ready, without blocking.
  pub(crate) fn poll(&self) -> Option<RawEvent> {
    let mut payload = [0u32; 2];
    // SAFETY: `payload` is valid for the two words the host writes.
    let kind = unsafe { imports::set_poll(self.0.get(), &mut payload) };
    (kind != EVENT_NONE).then_some(RawEvent {
      kind,
      waitable: payload[0],
      code: payload[1],
    })
  }
}

impl Drop for RawSet {
  fn drop(&mut self) {
    // SAFETY: the handle is owned and dropped once.
    unsafe { imports::set_drop(self.0.get()) }
  }
}

/// Declares async-lowered imports of one WASI interface as `mod imports`.
///
/// Off wasm there is no component-model host to call, so each import
/// becomes a function that panics: reaching one is misuse.
macro_rules! wasm_imports {
  ($module:literal {
    $( $link:literal fn $name:ident($($arg:ident: $ty:ty),* $(,)?) -> i32; )*
  }) => {
    #[cfg(target_arch = "wasm32")]
    mod imports {
      #[link(wasm_import_module = $module)]
      unsafe extern "C" {
        $(
          #[link_name = $link]
          pub(super) fn $name($($arg: $ty),*) -> i32;
        )*
      }
    }

    #[cfg(not(target_arch = "wasm32"))]
    mod imports {
      $(
        pub(super) unsafe fn $name($($arg: $ty),*) -> i32 {
          $( let _ = $arg; )*
          panic!("lio-wasi runs only inside a WebAssembly component")
        }
      )*
    }
  };
}

pub(crate) use wasm_imports;
