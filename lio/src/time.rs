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
use clock::TICK_MS;
pub use clock::{Clock, TimerId};

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
        epoch.elapsed().as_millis() as u64 / TICK_MS
      }
      TimeState::Paused => self.clock.current_tick(),
    }
  }

  fn deadline_ticks_after(&self, duration: Duration) -> u64 {
    match self.state {
      TimeState::Running { epoch } => {
        const TICK_NS: u128 = TICK_MS as u128 * 1_000_000;
        let elapsed_ns = epoch.elapsed().as_nanos();
        let deadline_ticks = elapsed_ns
          .saturating_add(duration.as_nanos())
          .div_ceil(TICK_NS)
          .min(u64::MAX as u128) as u64;
        deadline_ticks.max(self.clock.current_tick().saturating_add(1))
      }
      TimeState::Paused => self.clock.current_tick().saturating_add(
        duration
          .as_nanos()
          .div_ceil(TICK_MS as u128 * 1_000_000)
          .max(1)
          .min(u64::MAX as u128) as u64,
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

  /// Returns the duration until the next timer fires, if any.
  pub fn next_deadline(&self) -> Option<Duration> {
    self.clock.earliest_active_deadline().map(|deadline_ticks| {
      let now_ticks = self.now_ticks();
      if deadline_ticks <= now_ticks {
        Duration::ZERO
      } else {
        Duration::from_millis((deadline_ticks - now_ticks) * TICK_MS)
      }
    })
  }

  /// Polls for expired timers.
  pub fn poll_expired(&mut self) -> impl Iterator<Item = TimerId> + '_ {
    self.sync_to_now();
    self.clock.poll_expired()
  }

  /// Removes a timer from tracking.
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
        let elapsed =
          Duration::from_millis(self.clock.current_tick() * TICK_MS);
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
      let elapsed = Duration::from_millis(self.clock.current_tick() * TICK_MS);
      self.state = TimeState::Running { epoch: Instant::now() - elapsed };
    }
  }

  #[allow(dead_code)]
  pub fn is_paused(&self) -> bool {
    matches!(self.state, TimeState::Paused)
  }
}

/// Pause wall-clock advancement for the given [`Lio`] instance.
pub fn pause(lio: &Lio) {
  lio.pause_time();
}

/// Resume wall-clock advancement for the given [`Lio`] instance.
pub fn resume(lio: &Lio) {
  lio.resume_time();
}
