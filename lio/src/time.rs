//! Time management for lio.
//!
//! This module provides efficient timer management using a hashed hierarchical
//! timing wheel. The pure wheel logic lives in the internal `clock` module, while
//! the runtime uses an internal time manager to adapt it to wall-clock time.

mod clock;

#[cfg(test)]
mod tests;

use std::marker::PhantomData;
use std::time::{Duration, Instant};

use crate::Lio;
use crate::api::op::{Completion, CompletionFlags};
pub use clock::{Clock, TimerId};
use clock::{TICK_NS, ticks_duration};

/// Runtime-facing time manager that adapts [`Clock`] to wall-clock time.
#[derive(Debug)]
pub(crate) struct TimeManager {
  clock: Clock,
  state: TimeState,
  /// Marker to make this type `!Send`
  _not_send: PhantomData<*const ()>,
}

#[derive(Debug, Clone, Copy)]
enum TimeState {
  Running { epoch: Instant },
  Paused,
}

impl Default for TimeManager {
  fn default() -> Self {
    Self::new()
  }
}

impl TimeManager {
  /// Creates a new time manager.
  pub fn new() -> Self {
    Self::with_capacity(64)
  }

  /// Creates a time manager with pre-allocated capacity.
  pub fn with_capacity(capacity: usize) -> Self {
    Self {
      clock: Clock::with_capacity(capacity),
      state: TimeState::Running { epoch: Instant::now() },
      _not_send: PhantomData,
    }
  }

  fn now_ticks(&self) -> u64 {
    match self.state {
      TimeState::Running { epoch } => {
        (epoch.elapsed().as_nanos() / TICK_NS) as u64
      }
      TimeState::Paused => self.clock.current_tick(),
    }
  }

  fn deadline_ticks_after(&self, duration: Duration) -> u64 {
    match self.state {
      TimeState::Running { epoch } => {
        let elapsed_ns = epoch.elapsed().as_nanos();
        let deadline_ticks = elapsed_ns
          .saturating_add(duration.as_nanos())
          .div_ceil(TICK_NS)
          .min(u64::MAX as u128) as u64;
        deadline_ticks.max(self.clock.current_tick().saturating_add(1))
      }
      TimeState::Paused => self.clock.current_tick().saturating_add(
        duration.as_nanos().div_ceil(TICK_NS).max(1).min(u64::MAX as u128)
          as u64,
      ),
    }
  }

  fn sync_to_now(&mut self) {
    self.clock.advance_to(self.now_ticks());
  }

  /// Schedules a timer with the given ID and duration.
  pub fn schedule(&mut self, id: TimerId, duration: Duration) {
    self.sync_to_now();
    self.clock.schedule_at(id, self.deadline_ticks_after(duration));
  }

  /// Returns whether no active or pending timers are tracked.
  pub fn is_empty(&self) -> bool {
    self.clock.is_empty()
  }

  /// Returns the duration until the next timer fires, if any.
  ///
  /// Timers that already expired (moved out of the wheel by an earlier
  /// [`schedule`](Self::schedule)) but were not polled yet are due now.
  ///
  /// While paused, time only moves when advanced explicitly, so no wait
  /// makes a future deadline due: only timers already due count.
  pub fn next_deadline(&self) -> Option<Duration> {
    if self.clock.has_expired() {
      return Some(Duration::ZERO);
    }
    let deadline_ticks = self.clock.earliest_active_deadline()?;
    let now_ticks = self.now_ticks();
    if deadline_ticks <= now_ticks {
      Some(Duration::ZERO)
    } else if self.is_paused() {
      None
    } else {
      Some(ticks_duration(deadline_ticks - now_ticks))
    }
  }

  /// How long the driver may block for I/O without missing a timer: the
  /// caller's `timeout` (`None` for no limit), shortened to the next timer
  /// deadline.
  ///
  /// Doesn't read the clock when no timer is tracked, the common pure-I/O
  /// case.
  pub fn wait_timeout(&self, timeout: Option<Duration>) -> Option<Duration> {
    if self.is_empty() {
      return timeout;
    }
    match (timeout, self.next_deadline()) {
      (Some(user), Some(timer)) => Some(user.min(timer)),
      (user, timer) => user.or(timer),
    }
  }

  /// Polls for expired timers.
  ///
  /// Doesn't read the clock when no timer is tracked, including fired ones
  /// waiting to be drained.
  ///
  /// A tracked timer fires at most once per poll, so a poll that yields more
  /// timers than are tracked would never end; it panics instead.
  pub fn poll_expired(&mut self) -> impl Iterator<Item = TimerId> + '_ {
    if !self.is_empty() {
      self.sync_to_now();
    }
    let tracked = self.clock.len();
    at_most(tracked, self.clock.poll_expired())
  }

  /// The completion a fired timer is delivered with.
  ///
  /// Its result is always `0`: expiry is normalized here so op models never
  /// interpret OS-specific values for it.
  pub fn fired_completion() -> Completion {
    // SAFETY: the driver only routes this to registrations whose current
    // action is the sleep that fired, and this is what a sleep completes with.
    unsafe { Completion::with_flags(0, CompletionFlags::TIMER) }
  }

  /// Removes a timer from tracking.
  /// Whether timer `id` is still tracked: scheduled, or fired and not
  /// removed yet.
  pub fn contains(&self, id: TimerId) -> bool {
    self.clock.contains(id)
  }

  pub fn remove(&mut self, id: TimerId) -> bool {
    self.clock.remove(id)
  }

  pub fn advance_by_ticks(&mut self, ticks: u64) {
    if ticks == 0 {
      return;
    }
    match self.state {
      TimeState::Paused => self.clock.advance_by(ticks),
      TimeState::Running { .. } => {
        self.sync_to_now();
        self.clock.advance_by(ticks);
        let elapsed = ticks_duration(self.clock.current_tick());
        self.state = TimeState::Running { epoch: Instant::now() - elapsed };
      }
    }
  }

  pub fn pause(&mut self) {
    self.sync_to_now();
    self.state = TimeState::Paused;
  }

  pub fn resume(&mut self) {
    if matches!(self.state, TimeState::Paused) {
      let elapsed = ticks_duration(self.clock.current_tick());
      self.state = TimeState::Running { epoch: Instant::now() - elapsed };
    }
  }

  pub fn is_paused(&self) -> bool {
    matches!(self.state, TimeState::Paused)
  }
}

/// Yields what `timers` does, panicking once it yields more than `max`.
fn at_most(
  max: usize,
  timers: impl Iterator<Item = TimerId>,
) -> impl Iterator<Item = TimerId> {
  let mut left = max;
  timers.inspect(move |_| {
    assert!(left > 0, "clock: a poll fired more timers than the {max} tracked");
    left -= 1;
  })
}

/// Pause wall-clock advancement for the given [`Lio`] instance.
pub fn pause(lio: &Lio) {
  lio.pause_time();
}

/// Resume wall-clock advancement for the given [`Lio`] instance.
pub fn resume(lio: &Lio) {
  lio.resume_time();
}
