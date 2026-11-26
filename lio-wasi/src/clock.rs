//! Calls on `wasi:clocks/monotonic-clock@0.3.2`.

use core::time::Duration;

use crate::{Call, sealed};

/// Completes after a duration has passed, like a one-shot timer.
///
/// Push it with [`Queue::push_call`](crate::Queue::push_call). It has no
/// result: its completion reporting
/// [`Outcome::Returned`](crate::Outcome::Returned) is the timer firing.
/// Cancel it with [`Queue::cancel`](crate::Queue::cancel) to bound a
/// [`Queue::wait`](crate::Queue::wait) and then withdraw the bound.
#[derive(Clone, Copy, Debug)]
pub struct WaitFor {
  nanos: u64,
}

impl WaitFor {
  /// A timer for `how_long`, saturating at about 584 years.
  pub fn new(how_long: Duration) -> Self {
    Self { nanos: u64::try_from(how_long.as_nanos()).unwrap_or(u64::MAX) }
  }
}

impl Call for WaitFor {}

impl sealed::Sealed for WaitFor {
  unsafe fn start(&mut self) -> u32 {
    // The canonical ABI passes a `u64` as an `i64` with the same bits.
    let nanos = self.nanos.cast_signed();
    // SAFETY: the argument is passed by value and there is no result, so
    // the host keeps no pointer into this component.
    unsafe { imports::wait_for(nanos) as u32 }
  }
}

#[cfg(target_arch = "wasm32")]
mod imports {
  #[link(wasm_import_module = "wasi:clocks/monotonic-clock@0.3.2")]
  unsafe extern "C" {
    #[link_name = "[async-lower]wait-for"]
    pub(super) fn wait_for(how_long: i64) -> i32;
  }
}

/// See `sys::imports`: there is no host to call off wasm.
#[cfg(not(target_arch = "wasm32"))]
mod imports {
  pub(super) unsafe fn wait_for(_how_long: i64) -> i32 {
    panic!("lio-wasi runs only inside a WebAssembly component")
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn a_duration_is_counted_in_nanoseconds() {
    assert_eq!(WaitFor::new(Duration::from_millis(3)).nanos, 3_000_000);
  }

  #[test]
  fn a_duration_past_the_clock_range_saturates() {
    assert_eq!(WaitFor::new(Duration::MAX).nanos, u64::MAX);
  }
}
