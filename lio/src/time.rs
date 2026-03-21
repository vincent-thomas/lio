//! Time management for lio.
//!
//! This module provides efficient timer management using a hashed hierarchical
//! timing wheel. It's used internally by `Lio` to handle timeout operations
//! without creating a kernel timer per operation.
//!
//! # Design
//!
//! Uses a 4-level hierarchical timing wheel:
//! - Level 0: 256 slots × 1ms = 256ms range (near timers)
//! - Level 1: 256 slots × 256ms = 65.5s range
//! - Level 2: 256 slots × 65.5s = 4.66hr range
//! - Level 3: 256 slots × 4.66hr = 49.7d range (far timers)
//!
//! Operations:
//! - Schedule: O(1)
//! - Cancel: O(1)
//! - Expire: O(1) amortized per expired timer
//!
//! # Thread Safety
//!
//! `TimeManager` is `!Send` and `!Sync` - it's designed for single-threaded,
//! thread-per-core usage matching lio's design.

use std::collections::HashMap;
use std::marker::PhantomData;
use std::time::{Duration, Instant};

/// Unique identifier for a timer (matches operation ID).
pub type TimerId = u64;

/// State of a timer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TimerState {
  /// Timer is active and will fire at its deadline.
  Active,
  /// Timer has fired.
  Fired,
}

// Time resolution: 1ms per tick
const TICK_MS: u64 = 1;

/// Entry stored in a wheel slot.
#[derive(Debug, Clone, Copy)]
struct WheelEntry {
  id: TimerId,
  /// Absolute deadline in ticks from epoch.
  deadline_ticks: u64,
}

/// A single level of the timing wheel with compile-time size.
#[derive(Debug)]
struct Wheel<const SIZE: usize> {
  slots: [Vec<WheelEntry>; SIZE],
}

impl<const SIZE: usize> Wheel<SIZE> {
  fn new() -> Self {
    Self { slots: std::array::from_fn(|_| Vec::new()) }
  }

  fn insert(&mut self, slot: usize, entry: WheelEntry) {
    self.slots[slot % SIZE].push(entry);
  }

  fn take_slot(&mut self, slot: usize) -> Vec<WheelEntry> {
    std::mem::take(&mut self.slots[slot % SIZE])
  }

  fn clear(&mut self) {
    for slot in &mut self.slots {
      slot.clear();
    }
  }
}

/// Timer metadata for O(1) lookup.
#[derive(Debug)]
struct TimerEntry {
  /// Absolute deadline in ticks.
  deadline_ticks: u64,
  /// Current state.
  state: TimerState,
}

/// Number of bits per wheel level (256 slots).
const LEVEL_BITS: u32 = 8;
/// Number of slots per wheel level (levels 0-3).
const LEVEL_SIZE: usize = 1 << LEVEL_BITS; // 256
/// Bit mask for slot index.
const LEVEL_MASK: u64 = (LEVEL_SIZE - 1) as u64;

/// Manages all timer state for lio using a hierarchical timing wheel.
#[derive(Debug)]
pub struct TimeManager {
  /// Level 0: 256 slots × 1ms = 256ms
  level0: Wheel<LEVEL_SIZE>,
  /// Level 1: 256 slots × 256ms = 65.5s
  level1: Wheel<LEVEL_SIZE>,
  /// Level 2: 256 slots × 65.5s = 4.66hr
  level2: Wheel<LEVEL_SIZE>,
  /// Level 3: 256 slots × 4.66hr = 49.7d
  level3: Wheel<LEVEL_SIZE>,
  /// Current tick count (ms since epoch).
  current_tick: u64,
  /// Epoch instant for converting between ticks and real time.
  epoch: Instant,
  /// Map of timer ID to entry for O(1) lookup/cancel.
  timers: HashMap<TimerId, TimerEntry>,
  /// Timers ready to fire (collected during advance).
  pending_fire: Vec<TimerId>,
  /// Index into pending_fire for iteration.
  pending_index: usize,
  /// When paused, stores the instant when pause started.
  paused_at: Option<Instant>,
  /// Marker to make this type `!Send`
  _not_send: PhantomData<*const ()>,
}

impl Default for TimeManager {
  fn default() -> Self {
    Self::new()
  }
}

impl TimeManager {
  /// Range covered by each level in ticks (1 tick = 1ms).
  const LEVEL0_RANGE: u64 = LEVEL_SIZE as u64; // 256ms
  const LEVEL1_RANGE: u64 = Self::LEVEL0_RANGE * LEVEL_SIZE as u64; // 65.5s
  const LEVEL2_RANGE: u64 = Self::LEVEL1_RANGE * LEVEL_SIZE as u64; // 4.66hr

  /// Creates a new time manager.
  pub fn new() -> Self {
    Self::with_capacity(64)
  }

  /// Creates a time manager with pre-allocated capacity.
  pub fn with_capacity(capacity: usize) -> Self {
    Self {
      level0: Wheel::new(),
      level1: Wheel::new(),
      level2: Wheel::new(),
      level3: Wheel::new(),
      current_tick: 0,
      epoch: Instant::now(),
      timers: HashMap::with_capacity(capacity),
      pending_fire: Vec::with_capacity(32),
      pending_index: 0,
      paused_at: None,
      _not_send: PhantomData,
    }
  }

  /// Schedules a timer with the given ID and duration.
  pub fn schedule(&mut self, id: TimerId, duration: Duration) {
    let duration_ticks = duration.as_millis() as u64 / TICK_MS;
    let deadline_ticks = self.current_tick + duration_ticks.max(1);

    self.insert_timer(id, deadline_ticks);
    self
      .timers
      .insert(id, TimerEntry { deadline_ticks, state: TimerState::Active });
  }

  /// Insert a timer into the appropriate wheel level.
  fn insert_timer(&mut self, id: TimerId, deadline_ticks: u64) {
    let delta = deadline_ticks.saturating_sub(self.current_tick);
    let entry = WheelEntry { id, deadline_ticks };

    if delta < Self::LEVEL0_RANGE {
      let slot = (deadline_ticks & LEVEL_MASK) as usize;
      self.level0.insert(slot, entry);
    } else if delta < Self::LEVEL1_RANGE {
      let slot = ((deadline_ticks >> LEVEL_BITS) & LEVEL_MASK) as usize;
      self.level1.insert(slot, entry);
    } else if delta < Self::LEVEL2_RANGE {
      let slot = ((deadline_ticks >> (LEVEL_BITS * 2)) & LEVEL_MASK) as usize;
      self.level2.insert(slot, entry);
    } else {
      let slot = ((deadline_ticks >> (LEVEL_BITS * 3)) & LEVEL_MASK) as usize;
      self.level3.insert(slot, entry);
    }
  }

  /// Pauses all timer processing.
  ///
  /// While paused, `poll_expired()` returns no timers and `next_deadline()`
  /// returns `None`. When resumed, all timer deadlines are adjusted by the
  /// pause duration so no timers are lost.
  ///
  /// Returns `true` if the manager was running and is now paused.
  pub fn pause(&mut self) -> bool {
    if self.paused_at.is_none() {
      self.paused_at = Some(Instant::now());
      true
    } else {
      false
    }
  }

  /// Resumes timer processing after a pause.
  ///
  /// Timer deadlines are preserved relative to when they were scheduled.
  /// The epoch is shifted forward to account for paused time.
  ///
  /// Returns `true` if the manager was paused and is now running.
  pub fn resume(&mut self) -> bool {
    if let Some(paused_at) = self.paused_at.take() {
      let pause_ms = paused_at.elapsed().as_millis() as u64;

      // Shift epoch forward - this effectively "freezes" time during pause
      self.epoch += Duration::from_millis(pause_ms);

      // Rebuild wheels with existing deadlines (no adjustment needed)
      self.level0.clear();
      self.level1.clear();
      self.level2.clear();
      self.level3.clear();

      // Re-insert all active timers
      let active_timers: Vec<_> = self
        .timers
        .iter()
        .filter(|(_, e)| e.state == TimerState::Active)
        .map(|(&id, e)| (id, e.deadline_ticks))
        .collect();

      for (id, deadline_ticks) in active_timers {
        self.insert_timer(id, deadline_ticks);
      }

      true
    } else {
      false
    }
  }

  /// Returns `true` if the time manager is currently paused.
  pub fn is_paused(&self) -> bool {
    self.paused_at.is_some()
  }

  /// Returns the duration until the next timer fires, if any.
  ///
  /// Returns `None` if paused or no active timers.
  pub fn next_deadline(&self) -> Option<Duration> {
    if self.paused_at.is_some() {
      return None;
    }

    let mut earliest: Option<u64> = None;

    for entry in self.timers.values() {
      if entry.state == TimerState::Active {
        match earliest {
          None => earliest = Some(entry.deadline_ticks),
          Some(e) if entry.deadline_ticks < e => {
            earliest = Some(entry.deadline_ticks)
          }
          _ => {}
        }
      }
    }

    earliest.map(|deadline_ticks| {
      let now_ticks = self.now_ticks();
      if deadline_ticks <= now_ticks {
        Duration::ZERO
      } else {
        Duration::from_millis((deadline_ticks - now_ticks) * TICK_MS)
      }
    })
  }

  /// Get current time as ticks.
  fn now_ticks(&self) -> u64 {
    self.epoch.elapsed().as_millis() as u64 / TICK_MS
  }

  /// Polls for expired timers.
  ///
  /// Returns an iterator over timer IDs that have fired.
  /// Returns empty iterator if paused.
  pub fn poll_expired(&mut self) -> impl Iterator<Item = TimerId> + '_ {
    if self.paused_at.is_none() {
      self.advance_to_now();
    }
    ExpiredIterator { manager: self }
  }

  /// Advance the wheel to current time and collect expired timers.
  fn advance_to_now(&mut self) {
    let now_ticks = self.now_ticks();

    while self.current_tick < now_ticks {
      self.tick();
    }
  }

  /// Advance the wheel by one tick.
  fn tick(&mut self) {
    self.current_tick += 1;

    // Calculate current slot indices for each level
    let slot0 = (self.current_tick & LEVEL_MASK) as usize;
    let slot1 = ((self.current_tick >> LEVEL_BITS) & LEVEL_MASK) as usize;
    let slot2 = ((self.current_tick >> (LEVEL_BITS * 2)) & LEVEL_MASK) as usize;
    let slot3 = ((self.current_tick >> (LEVEL_BITS * 3)) & LEVEL_MASK) as usize;

    // Process level 0 current slot
    let entries = self.level0.take_slot(slot0);
    self.process_entries(entries);

    // Cascade from higher levels when their slot changes
    // Level 1 cascades when level 0 wraps (slot0 == 0)
    if slot0 == 0 {
      let entries = self.level1.take_slot(slot1);
      self.cascade_entries(entries);

      // Level 2 cascades when level 1 wraps
      if slot1 == 0 {
        let entries = self.level2.take_slot(slot2);
        self.cascade_entries(entries);

        // Level 3 cascades when level 2 wraps
        if slot2 == 0 {
          let entries = self.level3.take_slot(slot3);
          self.cascade_entries(entries);
        }
      }
    }
  }

  /// Process entries from level 0: fire expired, re-insert others.
  fn process_entries(&mut self, entries: Vec<WheelEntry>) {
    for entry in entries {
      let is_active = self
        .timers
        .get(&entry.id)
        .is_some_and(|t| t.state == TimerState::Active);

      if !is_active {
        continue;
      }

      if entry.deadline_ticks <= self.current_tick {
        // Timer has expired
        if let Some(timer) = self.timers.get_mut(&entry.id) {
          timer.state = TimerState::Fired;
        }
        self.pending_fire.push(entry.id);
      } else {
        // Re-insert (shouldn't happen often at level 0)
        self.insert_timer(entry.id, entry.deadline_ticks);
      }
    }
  }

  /// Cascade entries from higher levels down.
  fn cascade_entries(&mut self, entries: Vec<WheelEntry>) {
    for entry in entries {
      let is_active = self
        .timers
        .get(&entry.id)
        .is_some_and(|t| t.state == TimerState::Active);

      if !is_active {
        continue;
      }

      // Check if timer has already expired (deadline <= current tick)
      if entry.deadline_ticks <= self.current_tick {
        if let Some(timer) = self.timers.get_mut(&entry.id) {
          timer.state = TimerState::Fired;
        }
        self.pending_fire.push(entry.id);
      } else {
        self.insert_timer(entry.id, entry.deadline_ticks);
      }
    }
  }

  /// Pop next expired timer from pending list.
  fn pop_expired(&mut self) -> Option<TimerId> {
    if self.paused_at.is_some() {
      return None;
    }

    while self.pending_index < self.pending_fire.len() {
      let id = self.pending_fire[self.pending_index];
      self.pending_index += 1;

      if let Some(entry) = self.timers.get(&id)
        && entry.state == TimerState::Fired
      {
        return Some(id);
      }
    }

    // Reset pending list for next poll
    self.pending_fire.clear();
    self.pending_index = 0;
    None
  }

  /// Removes a timer from tracking.
  pub fn remove(&mut self, id: TimerId) -> bool {
    self.timers.remove(&id).is_some()
  }
}

/// Iterator over expired timers.
struct ExpiredIterator<'a> {
  manager: &'a mut TimeManager,
}

impl Iterator for ExpiredIterator<'_> {
  type Item = TimerId;

  fn next(&mut self) -> Option<Self::Item> {
    self.manager.pop_expired()
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::thread;

  #[test]
  fn test_schedule_and_poll() {
    let mut time = TimeManager::new();

    time.schedule(1, Duration::from_millis(1));

    thread::sleep(Duration::from_millis(5));

    let expired: Vec<_> = time.poll_expired().collect();
    assert_eq!(expired, vec![1]);
  }

  #[test]
  fn test_next_deadline() {
    let mut time = TimeManager::new();

    assert!(time.next_deadline().is_none());

    time.schedule(1, Duration::from_millis(100));

    let deadline = time.next_deadline();
    assert!(deadline.is_some());
    assert!(deadline.unwrap() <= Duration::from_millis(100));
  }

  #[test]
  fn test_pause_resume() {
    let mut time = TimeManager::new();

    time.schedule(1, Duration::from_millis(50));
    assert!(!time.is_paused());

    assert!(time.pause());
    assert!(time.is_paused());
    assert!(time.next_deadline().is_none());

    thread::sleep(Duration::from_millis(100));

    let expired: Vec<_> = time.poll_expired().collect();
    assert!(expired.is_empty());

    assert!(time.resume());
    assert!(!time.is_paused());

    let expired: Vec<_> = time.poll_expired().collect();
    assert!(expired.is_empty());

    thread::sleep(Duration::from_millis(60));
    let expired: Vec<_> = time.poll_expired().collect();
    assert_eq!(expired, vec![1]);
  }

  #[test]
  fn test_pause_resume_idempotent() {
    let mut time = TimeManager::new();

    assert!(!time.resume());
    assert!(time.pause());
    assert!(!time.pause());
    assert!(time.resume());
    assert!(!time.resume());
  }

  #[test]
  fn test_pause_resume_multiple_timers() {
    let mut time = TimeManager::new();

    time.schedule(1, Duration::from_millis(50));
    time.schedule(2, Duration::from_millis(100));
    time.schedule(3, Duration::from_millis(150));

    time.pause();
    thread::sleep(Duration::from_millis(200));

    assert!(time.poll_expired().next().is_none());

    time.resume();

    thread::sleep(Duration::from_millis(200));
    let expired: Vec<_> = time.poll_expired().collect();
    assert_eq!(expired, vec![1, 2, 3]);
  }

  #[test]
  fn test_ordering() {
    let mut time = TimeManager::new();

    time.schedule(3, Duration::from_millis(30));
    time.schedule(1, Duration::from_millis(10));
    time.schedule(2, Duration::from_millis(20));

    thread::sleep(Duration::from_millis(50));

    let expired: Vec<_> = time.poll_expired().collect();
    assert_eq!(expired, vec![1, 2, 3]);
  }

  #[test]
  fn test_remove() {
    let mut time = TimeManager::new();

    time.schedule(1, Duration::from_millis(1));
    thread::sleep(Duration::from_millis(5));

    let _: Vec<_> = time.poll_expired().collect();
    assert!(time.remove(1));
  }

  #[test]
  fn test_long_timer() {
    let mut time = TimeManager::new();

    time.schedule(1, Duration::from_millis(500));

    thread::sleep(Duration::from_millis(100));
    let expired: Vec<_> = time.poll_expired().collect();
    assert!(expired.is_empty());

    thread::sleep(Duration::from_millis(450));
    let expired: Vec<_> = time.poll_expired().collect();
    assert_eq!(expired, vec![1]);
  }

  #[test]
  fn test_many_timers() {
    let mut time = TimeManager::new();

    for i in 1..=100 {
      time.schedule(i, Duration::from_millis(i as u64));
    }

    thread::sleep(Duration::from_millis(150));
    let expired: Vec<_> = time.poll_expired().collect();
    assert_eq!(expired.len(), 100);

    for (i, &id) in expired.iter().enumerate() {
      assert_eq!(id, (i + 1) as u64);
    }
  }
}
