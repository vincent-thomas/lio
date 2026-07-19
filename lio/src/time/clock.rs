use std::collections::BTreeMap;
use std::time::Duration;

/// Unique identifier for a timer (matches operation ID).
pub type TimerId = u64;

/// Time resolution: 1ms per tick.
pub(crate) const TICK_MS: u64 = 1;
const TICK_NS: u128 = TICK_MS as u128 * 1_000_000;

const ACTIVE_TIMER: usize = usize::MAX;
const EMPTY_TIMER: usize = usize::MAX - 1;
const DELIVERED_TIMER: usize = usize::MAX - 2;

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
    self.slots[slot].push(entry);
  }

  fn take_slot(&mut self, slot: usize) -> Vec<WheelEntry> {
    std::mem::take(&mut self.slots[slot])
  }

  fn restore_slot(&mut self, slot: usize, entries: Vec<WheelEntry>) {
    let slot = &mut self.slots[slot];
    if slot.is_empty() {
      *slot = entries;
    }
  }
}

/// Timer metadata for O(1) lookup.
#[derive(Debug, Clone, Copy)]
struct TimerEntry {
  deadline_ticks: u64,
  /// Index in `pending_fire`, or `ACTIVE_TIMER` before expiration.
  pending_index: usize,
}

/// Occupant of one operation-store slot. The full generational ID rejects
/// stale wheel entries after the slot is reused.
#[derive(Debug, Clone, Copy)]
struct TimerSlot {
  id: TimerId,
  entry: TimerEntry,
}

impl TimerSlot {
  const EMPTY: Self = Self {
    id: 0,
    entry: TimerEntry { deadline_ticks: 0, pending_index: EMPTY_TIMER },
  };

  fn is_occupied(&self) -> bool {
    self.entry.pending_index != EMPTY_TIMER
  }
}

#[derive(Debug, Clone, Copy)]
struct PendingTimer {
  id: TimerId,
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
  timers: Vec<TimerSlot>,
  timer_count: usize,
  /// Earliest active deadline and the number of timers sharing it.
  earliest_deadline: Option<(u64, usize)>,
  /// Counts for active deadlines later than `earliest_deadline`.
  later_deadline_counts: BTreeMap<u64, usize>,
  pending_fire: Vec<PendingTimer>,
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
      timers: vec![TimerSlot::EMPTY; capacity],
      timer_count: 0,
      earliest_deadline: None,
      later_deadline_counts: BTreeMap::new(),
      pending_fire: Vec::with_capacity(32),
      pending_index: 0,
    }
  }

  #[inline]
  fn duration_to_ticks(duration: Duration) -> u64 {
    let nanos = duration.as_nanos();
    if nanos <= TICK_NS {
      return 1;
    }
    nanos.div_ceil(TICK_NS).min(u64::MAX as u128) as u64
  }

  #[inline]
  pub fn schedule(&mut self, id: TimerId, duration: Duration) {
    let duration_ticks = Self::duration_to_ticks(duration);
    let deadline_ticks = self.current_tick + duration_ticks.max(1);

    self.schedule_at(id, deadline_ticks);
  }

  #[inline]
  pub(crate) fn schedule_at(&mut self, id: TimerId, deadline_ticks: u64) {
    let deadline_ticks =
      deadline_ticks.max(self.current_tick.saturating_add(1));

    let slot_index = id as u32 as usize;
    if slot_index >= self.timers.len() {
      self.timers.resize(slot_index + 1, TimerSlot::EMPTY);
    }
    let entry =
      TimerEntry { deadline_ticks, pending_index: ACTIVE_TIMER };
    let previous =
      std::mem::replace(&mut self.timers[slot_index], TimerSlot { id, entry });

    if previous.is_occupied() {
      if previous.entry.pending_index == ACTIVE_TIMER {
        self.decrement_active_deadline(previous.entry.deadline_ticks);
      }
    } else {
      self.timer_count += 1;
    }
    self.insert_timer(id, deadline_ticks);
    self.increment_active_deadline(deadline_ticks);
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

  #[inline]
  pub fn poll_expired(&mut self) -> impl Iterator<Item = TimerId> + '_ {
    ExpiredIterator { clock: self }
  }

  #[inline]
  pub fn advance_to(&mut self, now_ticks: u64) {
    while self.current_tick < now_ticks {
      self.tick();
    }
  }

  #[inline]
  pub fn advance_by(&mut self, ticks: u64) {
    self.advance_to(self.current_tick.saturating_add(ticks));
  }

  pub fn remove(&mut self, id: TimerId) -> bool {
    let slot_index = id as u32 as usize;
    let Some(slot) = self.timers.get_mut(slot_index) else {
      return false;
    };
    if !slot.is_occupied() || slot.id != id {
      return false;
    }
    let timer = std::mem::replace(slot, TimerSlot::EMPTY);
    self.timer_count -= 1;
    if timer.entry.pending_index == ACTIVE_TIMER {
      self.decrement_active_deadline(timer.entry.deadline_ticks);
    }
    true
  }

  pub(crate) fn current_tick(&self) -> u64 {
    self.current_tick
  }

  pub(crate) fn is_empty(&self) -> bool {
    self.timer_count == 0
  }

  pub(crate) fn earliest_active_deadline(&self) -> Option<u64> {
    self.earliest_deadline.map(|(deadline, _)| deadline)
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

  fn increment_active_deadline(&mut self, deadline_ticks: u64) {
    match self.earliest_deadline {
      None => self.earliest_deadline = Some((deadline_ticks, 1)),
      Some((earliest, count)) if deadline_ticks == earliest => {
        self.earliest_deadline = Some((earliest, count + 1));
      }
      Some((earliest, count)) if deadline_ticks < earliest => {
        self.later_deadline_counts.insert(earliest, count);
        self.earliest_deadline = Some((deadline_ticks, 1));
      }
      Some(_) => {
        *self.later_deadline_counts.entry(deadline_ticks).or_insert(0) += 1;
      }
    }
  }

  fn decrement_active_deadline(&mut self, deadline_ticks: u64) {
    self.decrement_active_deadline_by(deadline_ticks, 1);
  }

  fn decrement_active_deadline_by(
    &mut self,
    deadline_ticks: u64,
    amount: usize,
  ) {
    match self.earliest_deadline {
      Some((earliest, count)) if deadline_ticks == earliest && count > amount => {
        self.earliest_deadline = Some((earliest, count - amount));
      }
      Some((earliest, _)) if deadline_ticks == earliest => {
        self.earliest_deadline = self.later_deadline_counts.pop_first();
      }
      _ => {
        let remove = match self.later_deadline_counts.get_mut(&deadline_ticks) {
          Some(count) if *count > amount => {
            *count -= amount;
            false
          }
          Some(_) => true,
          None => false,
        };
        if remove {
          self.later_deadline_counts.remove(&deadline_ticks);
        }
      }
    }
  }

  fn tick(&mut self) {
    self.current_tick += 1;

    let slot0 = (self.current_tick & LEVEL_MASK) as usize;
    self.expire_level0_slot(slot0);

    if slot0 == 0 {
      let slot1 =
        ((self.current_tick >> LEVEL_BITS) & LEVEL_MASK) as usize;
      let mut entries = self.level1.take_slot(slot1);
      self.process_entries(&mut entries);
      self.level1.restore_slot(slot1, entries);

      if slot1 == 0 {
        let slot2 =
          ((self.current_tick >> (LEVEL_BITS * 2)) & LEVEL_MASK) as usize;
        let mut entries = self.level2.take_slot(slot2);
        self.process_entries(&mut entries);
        self.level2.restore_slot(slot2, entries);

        if slot2 == 0 {
          let slot3 =
            ((self.current_tick >> (LEVEL_BITS * 3)) & LEVEL_MASK) as usize;
          let mut entries = self.level3.take_slot(slot3);
          self.process_entries(&mut entries);
          self.level3.restore_slot(slot3, entries);
        }
      }
    }
  }

  fn expire_level0_slot(&mut self, slot: usize) {
    let mut expired_count = 0;
    let current_tick = self.current_tick;
    let entries = &mut self.level0.slots[slot];
    let timers = &mut self.timers;
    let pending_fire = &mut self.pending_fire;

    for entry in entries.drain(..) {
      let slot_index = entry.id as u32 as usize;
      let Some(timer) = timers.get_mut(slot_index) else {
        continue;
      };
      if timer.id != entry.id
        || timer.entry.pending_index != ACTIVE_TIMER
        || timer.entry.deadline_ticks != entry.deadline_ticks
      {
        continue;
      }

      debug_assert!(entry.deadline_ticks <= current_tick);
      timer.entry.pending_index = pending_fire.len();
      expired_count += 1;
      pending_fire.push(PendingTimer { id: entry.id });
    }

    if expired_count != 0 {
      self.decrement_active_deadline_by(current_tick, expired_count);
    }
  }

  fn process_entries(&mut self, entries: &mut Vec<WheelEntry>) {
    let mut expired_count = 0;
    for entry in entries.drain(..) {
      let slot_index = entry.id as u32 as usize;
      let Some(timer) = self.timers.get_mut(slot_index) else {
        continue;
      };
      if timer.id != entry.id
        || timer.entry.pending_index != ACTIVE_TIMER
        || timer.entry.deadline_ticks != entry.deadline_ticks
      {
        continue;
      }

      if entry.deadline_ticks <= self.current_tick {
        timer.entry.pending_index = self.pending_fire.len();
        expired_count += 1;
        self.pending_fire.push(PendingTimer { id: entry.id });
      } else {
        self.insert_timer(entry.id, entry.deadline_ticks);
      }
    }

    if expired_count != 0 {
      // Active entries can only become due in the slot for the current tick.
      self.decrement_active_deadline_by(self.current_tick, expired_count);
    }
  }

  #[inline]
  fn pop_expired(&mut self) -> Option<TimerId> {
    while self.pending_index < self.pending_fire.len() {
      let pending = self.pending_fire[self.pending_index];
      self.pending_index += 1;

      let slot_index = pending.id as u32 as usize;
      if let Some(timer) = self.timers.get_mut(slot_index)
        && timer.id == pending.id
        && timer.entry.pending_index == self.pending_index - 1
      {
        timer.entry.pending_index = DELIVERED_TIMER;
        return Some(pending.id);
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

  #[inline]
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
  fn rescheduling_active_timer_replaces_deadline() {
    let mut clock = Clock::new();
    clock.schedule(1, Duration::from_millis(2));
    clock.schedule(1, Duration::from_millis(5));

    clock.advance_by(2);
    assert!(collect_expired(&mut clock).is_empty());

    clock.advance_by(3);
    assert_eq!(collect_expired(&mut clock), vec![1]);
  }

  #[test]
  fn rescheduling_fired_timer_suppresses_pending_expiration() {
    let mut clock = Clock::new();
    clock.schedule(1, Duration::from_millis(1));
    clock.advance_by(1);

    clock.schedule(1, Duration::from_millis(1));
    assert!(collect_expired(&mut clock).is_empty());

    clock.advance_by(1);
    assert_eq!(collect_expired(&mut clock), vec![1]);
  }

  #[test]
  fn stale_pending_index_does_not_cancel_another_timer() {
    let mut clock = Clock::new();
    clock.schedule(1, Duration::from_millis(1));
    clock.advance_by(1);
    assert_eq!(collect_expired(&mut clock), vec![1]);

    clock.schedule(2, Duration::from_millis(1));
    clock.advance_by(1);
    assert!(clock.remove(1));
    assert_eq!(collect_expired(&mut clock), vec![2]);
  }

  #[test]
  fn newer_generation_replaces_timer_in_the_same_slot() {
    let mut clock = Clock::new();
    let old_id = 7;
    let new_id = (1_u64 << 32) | 7;
    clock.schedule(old_id, Duration::from_millis(1));
    clock.schedule(new_id, Duration::from_millis(2));

    clock.advance_by(1);
    assert!(collect_expired(&mut clock).is_empty());
    assert!(!clock.remove(old_id));

    clock.advance_by(1);
    assert_eq!(collect_expired(&mut clock), vec![new_id]);
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
  fn timers_expire_across_level0_boundary() {
    let mut clock = Clock::new();
    clock.schedule(1, Duration::from_millis(255));
    clock.schedule(2, Duration::from_millis(256));
    clock.schedule(3, Duration::from_millis(257));

    clock.advance_by(257);
    assert_eq!(collect_expired(&mut clock), vec![1, 2, 3]);
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
