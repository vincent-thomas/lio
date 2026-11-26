//! Calls on `wasi:clocks/monotonic-clock@0.3.2`.

use core::time::Duration;

use crate::Call;
use crate::sealed::{self, Started};
use crate::sys::wasm_imports;

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
  unsafe fn start(&mut self) -> Started {
    // The canonical ABI passes a `u64` as an `i64` with the same bits.
    let nanos = self.nanos.cast_signed();
    // SAFETY: the argument is passed by value and there is no result, so
    // the host keeps no pointer into this component.
    Started::Subtask(unsafe { imports::wait_for(nanos) } as u32)
  }
}

/// Completes when the monotonic clock reaches a mark, like an absolute
/// timer.
///
/// A mark is a count of nanoseconds on the clock that
/// `wasip3::clocks::monotonic_clock::now` reads. Like [`WaitFor`], it has no
/// result.
#[derive(Clone, Copy, Debug)]
pub struct WaitUntil {
  mark: u64,
}

impl WaitUntil {
  /// A timer for the moment the clock reads `mark`.
  pub fn new(mark: u64) -> Self {
    Self { mark }
  }
}

impl Call for WaitUntil {}

impl sealed::Sealed for WaitUntil {
  unsafe fn start(&mut self) -> Started {
    // SAFETY: as for `WaitFor`.
    Started::Subtask(
      unsafe { imports::wait_until(self.mark.cast_signed()) } as u32
    )
  }
}

wasm_imports!("wasi:clocks/monotonic-clock@0.3.2" {
  "[async-lower]wait-for" fn wait_for(how_long: i64) -> i32;
  "[async-lower]wait-until" fn wait_until(when: i64) -> i32;
});

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
