use std::collections::HashMap;
use std::time::Duration;

/// Unique identifier for a timer (matches operation ID).
pub type TimerId = u64;

/// Time resolution: 1ms per tick.
pub(crate) const TICK_MS: u64 = 1;
const TICK_NS: u128 = TICK_MS as u128 * 1_000_000;

/// State of a timer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TimerState {
  Active,
  Fired,
}

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
}

/// Timer metadata for O(1) lookup.
#[derive(Debug)]
struct TimerEntry {
  deadline_ticks: u64,
  state: TimerState,
}

/// Number of bits per wheel level (256 slots).
const LEVEL_BITS: u32 = 8;
/// Number of slots per wheel level (levels 0-3).
const LEVEL_SIZE: usize = 1 << LEVEL_BITS;
/// Bit mask for slot index.
const LEVEL_MASK: u64 = (LEVEL_SIZE - 1) as u64;

/// Pure timer-wheel clock with deterministic, manually-advanced time.
#[derive(Debug)]
pub struct Clock {
  level0: Wheel<LEVEL_SIZE>,
  level1: Wheel<LEVEL_SIZE>,
  level2: Wheel<LEVEL_SIZE>,
  level3: Wheel<LEVEL_SIZE>,
  /// Current logical tick count.
  current_tick: u64,
  timers: HashMap<TimerId, TimerEntry>,
  pending_fire: Vec<TimerId>,
  pending_index: usize,
}

impl Default for Clock {
  fn default() -> Self {
    Self::new()
  }
}

impl Clock {
  const LEVEL0_RANGE: u64 = LEVEL_SIZE as u64;
  const LEVEL1_RANGE: u64 = Self::LEVEL0_RANGE * LEVEL_SIZE as u64;
  const LEVEL2_RANGE: u64 = Self::LEVEL1_RANGE * LEVEL_SIZE as u64;

  pub fn new() -> Self {
    Self::with_capacity(64)
  }

  pub fn with_capacity(capacity: usize) -> Self {
    Self {
      level0: Wheel::new(),
      level1: Wheel::new(),
      level2: Wheel::new(),
      level3: Wheel::new(),
      current_tick: 0,
      timers: HashMap::with_capacity(capacity),
      pending_fire: Vec::with_capacity(32),
      pending_index: 0,
    }
  }

  fn duration_to_ticks(duration: Duration) -> u64 {
    let ticks = duration.as_nanos().div_ceil(TICK_NS).max(1);
    ticks.min(u64::MAX as u128) as u64
  }

  pub fn schedule(&mut self, id: TimerId, duration: Duration) {
    let duration_ticks = Self::duration_to_ticks(duration);
    let deadline_ticks = self.current_tick + duration_ticks.max(1);

    self.insert_timer(id, deadline_ticks);
    self
      .timers
      .insert(id, TimerEntry { deadline_ticks, state: TimerState::Active });
  }

  pub fn next_deadline(&self) -> Option<Duration> {
    self.earliest_active_deadline().map(|deadline_ticks| {
      if deadline_ticks <= self.current_tick {
        Duration::ZERO
      } else {
        Duration::from_millis((deadline_ticks - self.current_tick) * TICK_MS)
      }
    })
  }

  pub fn poll_expired(&mut self) -> impl Iterator<Item = TimerId> + '_ {
    ExpiredIterator { clock: self }
  }

  pub fn advance_to(&mut self, now_ticks: u64) {
    while self.current_tick < now_ticks {
      self.tick();
    }
  }

  pub fn advance_by(&mut self, ticks: u64) {
    self.advance_to(self.current_tick.saturating_add(ticks));
  }

  pub fn remove(&mut self, id: TimerId) -> bool {
    self.timers.remove(&id).is_some()
  }

  pub(crate) fn current_tick(&self) -> u64 {
    self.current_tick
  }

  pub(crate) fn earliest_active_deadline(&self) -> Option<u64> {
    self
      .timers
      .values()
      .filter(|entry| entry.state == TimerState::Active)
      .map(|entry| entry.deadline_ticks)
      .min()
  }

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

  fn tick(&mut self) {
    self.current_tick += 1;

    let slot0 = (self.current_tick & LEVEL_MASK) as usize;
    let slot1 = ((self.current_tick >> LEVEL_BITS) & LEVEL_MASK) as usize;
    let slot2 = ((self.current_tick >> (LEVEL_BITS * 2)) & LEVEL_MASK) as usize;
    let slot3 = ((self.current_tick >> (LEVEL_BITS * 3)) & LEVEL_MASK) as usize;

    let entries = self.level0.take_slot(slot0);
    self.process_entries(entries);

    if slot0 == 0 {
      let entries = self.level1.take_slot(slot1);
      self.cascade_entries(entries);

      if slot1 == 0 {
        let entries = self.level2.take_slot(slot2);
        self.cascade_entries(entries);

        if slot2 == 0 {
          let entries = self.level3.take_slot(slot3);
          self.cascade_entries(entries);
        }
      }
    }
  }

  fn process_entries(&mut self, entries: Vec<WheelEntry>) {
    for entry in entries {
      let is_active = self
        .timers
        .get(&entry.id)
        .is_some_and(|timer| timer.state == TimerState::Active);

      if !is_active {
        continue;
      }

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

  fn cascade_entries(&mut self, entries: Vec<WheelEntry>) {
    for entry in entries {
      let is_active = self
        .timers
        .get(&entry.id)
        .is_some_and(|timer| timer.state == TimerState::Active);

      if !is_active {
        continue;
      }

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

  fn pop_expired(&mut self) -> Option<TimerId> {
    while self.pending_index < self.pending_fire.len() {
      let id = self.pending_fire[self.pending_index];
      self.pending_index += 1;

      if let Some(entry) = self.timers.get(&id)
        && entry.state == TimerState::Fired
      {
        return Some(id);
      }
    }

    self.pending_fire.clear();
    self.pending_index = 0;
    None
  }
}

struct ExpiredIterator<'a> {
  clock: &'a mut Clock,
}

impl Iterator for ExpiredIterator<'_> {
  type Item = TimerId;

  fn next(&mut self) -> Option<Self::Item> {
    self.clock.pop_expired()
  }
}

#[cfg(test)]
mod tests {
  use std::time::Duration;

  use crate::time::{Clock, TimerId};

  fn collect_expired(clock: &mut Clock) -> Vec<TimerId> {
    clock.poll_expired().collect()
  }

  #[test]
  fn schedule_and_poll_after_manual_advance() {
    let mut clock = Clock::new();
    clock.schedule(1, Duration::from_millis(1));

    assert!(collect_expired(&mut clock).is_empty());

    clock.advance_by(1);
    assert_eq!(collect_expired(&mut clock), vec![1]);
    assert!(collect_expired(&mut clock).is_empty());
  }

  #[test]
  fn next_deadline_uses_logical_tick() {
    let mut clock = Clock::new();
    assert_eq!(clock.next_deadline(), None);

    clock.schedule(1, Duration::from_millis(10));
    assert_eq!(clock.next_deadline(), Some(Duration::from_millis(10)));

    clock.advance_by(4);
    assert_eq!(clock.next_deadline(), Some(Duration::from_millis(6)));

    clock.advance_by(6);
    assert_eq!(clock.next_deadline(), None);
  }

  #[test]
  fn expires_in_deadline_order() {
    let mut clock = Clock::new();

    clock.schedule(3, Duration::from_millis(30));
    clock.schedule(1, Duration::from_millis(10));
    clock.schedule(2, Duration::from_millis(20));

    clock.advance_by(30);
    assert_eq!(collect_expired(&mut clock), vec![1, 2, 3]);
  }

  #[test]
  fn remove_prevents_future_fire() {
    let mut clock = Clock::new();
    clock.schedule(1, Duration::from_millis(5));
    assert!(clock.remove(1));

    clock.advance_by(10);
    assert!(collect_expired(&mut clock).is_empty());
  }

  #[test]
  fn remove_after_firing_but_before_drain_suppresses_delivery() {
    let mut clock = Clock::new();
    clock.schedule(1, Duration::from_millis(1));

    clock.advance_by(1);
    assert!(clock.remove(1));
    assert!(collect_expired(&mut clock).is_empty());
  }

  #[test]
  fn far_timer_does_not_fire_early() {
    let mut clock = Clock::new();
    clock.schedule(1, Duration::from_millis(500));

    clock.advance_by(499);
    assert!(collect_expired(&mut clock).is_empty());

    clock.advance_by(1);
    assert_eq!(collect_expired(&mut clock), vec![1]);
  }

  #[test]
  fn many_timers_all_fire_once() {
    let mut clock = Clock::new();

    for i in 1..=100 {
      clock.schedule(i, Duration::from_millis(i));
    }

    clock.advance_by(100);
    let expired = collect_expired(&mut clock);
    assert_eq!(expired.len(), 100);

    for (i, &id) in expired.iter().enumerate() {
      assert_eq!(id, (i + 1) as u64);
    }

    assert!(collect_expired(&mut clock).is_empty());
  }

  #[test]
  fn zero_duration_still_waits_one_tick() {
    let mut clock = Clock::new();
    clock.schedule(1, Duration::ZERO);

    assert!(collect_expired(&mut clock).is_empty());
    clock.advance_by(1);
    assert_eq!(collect_expired(&mut clock), vec![1]);
  }
}
