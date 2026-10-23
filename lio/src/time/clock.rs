use std::collections::BTreeMap;
use std::time::Duration;

/// Unique identifier for a timer (matches operation ID).
pub type TimerId = u64;

/// Time resolution: 1ms per tick.
pub(crate) const TICK_MS: u64 = 1;
pub(crate) const TICK_NS: u128 = TICK_MS as u128 * 1_000_000;

/// The time `ticks` ticks span.
#[inline]
pub(crate) fn ticks_duration(ticks: u64) -> Duration {
  Duration::from_millis(ticks * TICK_MS)
}

const ACTIVE_TIMER: usize = usize::MAX;
const EMPTY_TIMER: usize = usize::MAX - 1;

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
  /// Entries across all slots, so advancing can skip empty levels.
  len: usize,
}

impl<const SIZE: usize> Wheel<SIZE> {
  fn new() -> Self {
    Self { slots: std::array::from_fn(|_| Vec::new()), len: 0 }
  }

  fn insert(&mut self, slot: usize, entry: WheelEntry) {
    self.slots[slot].push(entry);
    self.len += 1;
  }

  fn take_slot(&mut self, slot: usize) -> Vec<WheelEntry> {
    let entries = std::mem::take(&mut self.slots[slot]);
    self.len -= entries.len();
    entries
  }

  /// Gives a drained slot's allocation back for reuse.
  fn restore_slot(&mut self, slot: usize, entries: Vec<WheelEntry>) {
    assert!(entries.is_empty());
    let slot = &mut self.slots[slot];
    if slot.is_empty() {
      *slot = entries;
    }
  }

  fn clear_slot(&mut self, slot: usize) {
    self.len -= self.slots[slot].len();
    self.slots[slot].clear();
  }

  /// Keeps only the entries `live` accepts.
  fn retain(&mut self, mut live: impl FnMut(&WheelEntry) -> bool) {
    for slot in &mut self.slots {
      if !slot.is_empty() {
        slot.retain(&mut live);
      }
    }
    self.len = self.slots.iter().map(Vec::len).sum();
  }
}

/// Timer metadata for O(1) lookup.
#[derive(Debug, Clone, Copy)]
struct TimerEntry {
  deadline_ticks: u64,
  /// Index assigned in `pending_fire`, or `ACTIVE_TIMER` before expiration.
  /// Whether that index is still pending is determined by the drain cursor.
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
  pending_has_stale: bool,
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
      pending_has_stale: false,
    }
  }

  #[inline]
  fn duration_to_ticks(duration: Duration) -> u64 {
    let nanos = duration.as_nanos();
    if nanos <= TICK_NS {
      return 1;
    }
    Self::long_duration_to_ticks(nanos)
  }

  #[cold]
  #[inline(never)]
  fn long_duration_to_ticks(nanos: u128) -> u64 {
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
      // Checked: on 32-bit targets `u32::MAX + 1` would wrap to 0.
      #[expect(clippy::expect_used, reason = "no memory holds 2^32 timers")]
      let required = slot_index.checked_add(1).expect("timer id out of range");
      self.grow_timer_slots(required);
    }
    let entry = TimerEntry { deadline_ticks, pending_index: ACTIVE_TIMER };
    let previous =
      std::mem::replace(&mut self.timers[slot_index], TimerSlot { id, entry });

    let previous_state = previous.entry.pending_index;
    if previous_state >= EMPTY_TIMER {
      if previous_state == EMPTY_TIMER {
        self.activate_timer_slot();
      } else {
        self.replace_active_timer(previous.entry.deadline_ticks);
        // The previous deadline's wheel entry is stale now.
        self.compact_if_mostly_stale();
      }
    } else if self.pending_is_undelivered(previous.id, previous_state) {
      self.pending_has_stale = true;
    }
    self.insert_timer(id, deadline_ticks);
    self.increment_active_deadline(deadline_ticks);
  }

  #[cold]
  #[inline(never)]
  fn grow_timer_slots(&mut self, required_len: usize) {
    // Never shrinks: wheel and pending entries index slots unchecked.
    if required_len > self.timers.len() {
      self.timers.resize(required_len, TimerSlot::EMPTY);
    }
  }

  #[cold]
  #[inline(never)]
  fn replace_active_timer(&mut self, previous_deadline: u64) {
    self.decrement_active_deadline(previous_deadline);
  }

  #[cold]
  #[inline(never)]
  fn activate_timer_slot(&mut self) {
    self.timer_count += 1;
  }

  pub fn next_deadline(&self) -> Option<Duration> {
    self.earliest_active_deadline().map(|deadline_ticks| {
      if deadline_ticks <= self.current_tick {
        Duration::ZERO
      } else {
        ticks_duration(deadline_ticks - self.current_tick)
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
      // Ticks before the next one that processes a non-empty slot change
      // nothing, so skip them instead of stepping one millisecond at a time.
      let Some(next) = self.next_busy_tick() else {
        self.current_tick = now_ticks;
        break;
      };
      if next > now_ticks {
        self.current_tick = now_ticks;
        break;
      }
      self.current_tick = next - 1;
      self.tick();
    }
    // Every timer due by now has fired, so its deadline is no longer
    // counted. One still counted lost its wheel entry and never will.
    if let Some((deadline, count)) = self.earliest_deadline {
      assert!(
        deadline > self.current_tick,
        "clock: {count} timers due at tick {deadline} did not fire by tick \
         {}: their wheel entry is lost",
        self.current_tick
      );
    }
  }

  /// The first tick after the current one whose [`tick`](Self::tick)
  /// processes a slot that holds entries, or `None` if the wheel is empty.
  ///
  /// `tick` processes the level-0 slot every tick and a level-`k` slot only
  /// when the tick is a multiple of `256^k`, so with the lower levels empty
  /// nothing happens before the next such multiple.
  fn next_busy_tick(&self) -> Option<u64> {
    let lens =
      [self.level0.len, self.level1.len, self.level2.len, self.level3.len];
    let level = lens.iter().position(|&len| len != 0)?;
    let period = 1u64 << (LEVEL_BITS * level as u32);
    Some((self.current_tick / period).saturating_add(1).saturating_mul(period))
  }

  #[inline]
  pub fn advance_by(&mut self, ticks: u64) {
    self.advance_to(self.current_tick.saturating_add(ticks));
  }

  /// Whether timer `id` is still tracked: scheduled, or fired and not
  /// removed yet.
  pub(crate) fn contains(&self, id: TimerId) -> bool {
    self
      .timers
      .get(id as u32 as usize)
      .is_some_and(|slot| slot.is_occupied() && slot.id == id)
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
      // Its wheel entry stays behind until its slot comes round.
      self.compact_if_mostly_stale();
    } else if self.pending_is_undelivered(timer.id, timer.entry.pending_index) {
      self.pending_has_stale = true;
    }
    true
  }

  /// Wheel entries of removed or rescheduled timers stay in their slot until
  /// it is processed, which for a long timeout can be far off. Drop them
  /// once they outnumber the timers still tracked, so memory stays bounded
  /// by the live timers. The sweep costs about the entries plus the slots, so
  /// spread over the removals that triggered it, each costs O(1).
  fn compact_if_mostly_stale(&mut self) {
    const FLOOR: usize = 4 * LEVEL_SIZE;
    let entries =
      self.level0.len + self.level1.len + self.level2.len + self.level3.len;
    if entries <= 2 * self.timer_count + FLOOR {
      return;
    }
    let timers = &self.timers;
    let live = |entry: &WheelEntry| {
      timers.get(entry.id as u32 as usize).is_some_and(|timer| {
        timer.id == entry.id
          && timer.entry.pending_index == ACTIVE_TIMER
          && timer.entry.deadline_ticks == entry.deadline_ticks
      })
    };
    self.level0.retain(live);
    self.level1.retain(live);
    self.level2.retain(live);
    self.level3.retain(live);
  }

  pub(crate) fn current_tick(&self) -> u64 {
    self.current_tick
  }

  pub(crate) fn is_empty(&self) -> bool {
    self.timer_count == 0
  }

  /// Number of timers tracked: scheduled, or fired and not removed yet.
  pub(crate) fn len(&self) -> usize {
    self.timer_count
  }

  /// Returns whether timers have expired but not been handed out by
  /// [`poll_expired`](Self::poll_expired) yet.
  pub(crate) fn has_expired(&self) -> bool {
    self.pending_index < self.pending_fire.len()
  }

  pub(crate) fn earliest_active_deadline(&self) -> Option<u64> {
    self.earliest_deadline.map(|(deadline, _)| deadline)
  }

  #[inline]
  fn insert_timer(&mut self, id: TimerId, deadline_ticks: u64) {
    let delta = deadline_ticks.saturating_sub(self.current_tick);
    let entry = WheelEntry { id, deadline_ticks };

    if delta < Self::LEVEL0_RANGE {
      let slot = (deadline_ticks & LEVEL_MASK) as usize;
      self.level0.insert(slot, entry);
    } else {
      self.insert_higher_timer(delta, entry);
    }
  }

  #[cold]
  #[inline(never)]
  fn insert_higher_timer(&mut self, delta: u64, entry: WheelEntry) {
    if delta < Self::LEVEL1_RANGE {
      let slot = ((entry.deadline_ticks >> LEVEL_BITS) & LEVEL_MASK) as usize;
      self.level1.insert(slot, entry);
    } else if delta < Self::LEVEL2_RANGE {
      let slot =
        ((entry.deadline_ticks >> (LEVEL_BITS * 2)) & LEVEL_MASK) as usize;
      self.level2.insert(slot, entry);
    } else {
      let slot =
        ((entry.deadline_ticks >> (LEVEL_BITS * 3)) & LEVEL_MASK) as usize;
      self.level3.insert(slot, entry);
    }
  }

  #[inline]
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
      Some((earliest, count))
        if deadline_ticks == earliest && count > amount =>
      {
        self.earliest_deadline = Some((earliest, count - amount));
      }
      Some((earliest, count)) if deadline_ticks == earliest => {
        assert_eq!(
          count, amount,
          "clock: {amount} timers left deadline {deadline_ticks}, which only \
           {count} had"
        );
        self.earliest_deadline = self.later_deadline_counts.pop_first();
      }
      _ => {
        let Some(count) = self.later_deadline_counts.get_mut(&deadline_ticks)
        else {
          panic!(
            "clock: {amount} timers left deadline {deadline_ticks}, which none \
             had"
          );
        };
        assert!(
          *count >= amount,
          "clock: {amount} timers left deadline {deadline_ticks}, which only \
           {count} had"
        );
        *count -= amount;
        if *count == 0 {
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
      let slot1 = ((self.current_tick >> LEVEL_BITS) & LEVEL_MASK) as usize;
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
    let entries = &self.level0.slots[slot];
    let timers = &mut self.timers;
    let pending_fire = &mut self.pending_fire;

    for &entry in entries.iter() {
      let slot_index = entry.id as u32 as usize;
      // SAFETY: wheel entries are only created after their timer slot exists,
      // and the timer-slot vector never shrinks.
      let timer = unsafe { timers.get_unchecked_mut(slot_index) };
      if timer.id != entry.id
        || timer.entry.pending_index != ACTIVE_TIMER
        || timer.entry.deadline_ticks != entry.deadline_ticks
      {
        continue;
      }

      assert!(entry.deadline_ticks <= current_tick);
      timer.entry.pending_index = pending_fire.len();
      expired_count += 1;
      pending_fire.push(PendingTimer { id: entry.id });
    }
    self.level0.clear_slot(slot);

    if expired_count != 0 {
      self.decrement_active_deadline_by(current_tick, expired_count);
    }
  }

  fn process_entries(&mut self, entries: &mut Vec<WheelEntry>) {
    let mut expired_count = 0;
    for entry in entries.drain(..) {
      let slot_index = entry.id as u32 as usize;
      // SAFETY: wheel entries are only created after their timer slot exists,
      // and the timer-slot vector never shrinks.
      let timer = unsafe { self.timers.get_unchecked_mut(slot_index) };
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

  /// Whether fired entry `index` is still its timer's current delivery.
  fn pending_is_live(&self, index: usize) -> bool {
    let pending = self.pending_fire[index];
    self.timers.get(pending.id as u32 as usize).is_some_and(|timer| {
      timer.id == pending.id && timer.entry.pending_index == index
    })
  }

  #[inline]
  fn pending_is_undelivered(&self, id: TimerId, index: usize) -> bool {
    index >= self.pending_index
      && self.pending_fire.get(index).is_some_and(|timer| timer.id == id)
  }

  #[inline]
  fn pop_expired(&mut self) -> Option<TimerId> {
    while self.pending_index < self.pending_fire.len() {
      let pending = self.pending_fire[self.pending_index];
      let index = self.pending_index;
      self.pending_index += 1;

      if !self.pending_has_stale {
        // The flag lets delivery skip the stale check; verify it was right.
        assert!(
          self.pending_is_live(index),
          "clock: delivering stale timer {}, which is not marked stale",
          pending.id
        );
        return Some(pending.id);
      }

      let slot_index = pending.id as u32 as usize;
      // SAFETY: pending entries are only created after their timer slot exists,
      // and the timer-slot vector never shrinks.
      let timer = unsafe { self.timers.get_unchecked(slot_index) };
      if timer.id == pending.id && timer.entry.pending_index == index {
        return Some(pending.id);
      }
    }

    self.pending_fire.clear();
    self.pending_index = 0;
    self.pending_has_stale = false;
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

  #[inline]
  fn fold<B, F>(mut self, mut accum: B, mut f: F) -> B
  where
    F: FnMut(B, Self::Item) -> B,
  {
    if self.clock.pending_has_stale {
      for id in self.by_ref() {
        accum = f(accum, id);
      }
      return accum;
    }

    while self.clock.pending_index < self.clock.pending_fire.len() {
      let pending = self.clock.pending_fire[self.clock.pending_index];
      assert!(
        self.clock.pending_is_live(self.clock.pending_index),
        "clock: delivering stale timer {}, which is not marked stale",
        pending.id
      );
      self.clock.pending_index += 1;

      accum = f(accum, pending.id);
    }

    self.clock.pending_fire.clear();
    self.clock.pending_index = 0;
    accum
  }
}

#[cfg(test)]
mod tests {
  use std::time::Duration;

  use super::{TICK_MS, Wheel};
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
  fn partial_drain_distinguishes_consumed_and_pending_timers() {
    let mut clock = Clock::new();
    clock.schedule(1, Duration::from_millis(1));
    clock.schedule(2, Duration::from_millis(1));
    clock.advance_by(1);

    let mut expired = clock.poll_expired();
    assert_eq!(expired.next(), Some(1));
    drop(expired);

    clock.schedule(1, Duration::from_millis(1));
    assert!(clock.remove(2));
    assert!(collect_expired(&mut clock).is_empty());

    clock.advance_by(1);
    assert_eq!(collect_expired(&mut clock), vec![1]);
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

  /// Every wheel level, and the boundaries between them, fire exactly on the
  /// deadline when the clock skips idle ticks.
  #[test]
  fn skipping_idle_ticks_fires_every_level_on_time() {
    let deadlines: [u64; 10] = [
      1,
      255,
      256,
      257,
      65_535,
      65_536,
      70_000,
      (1 << 24) + 5,
      (1 << 32) + 7,
      (1 << 33) + 300,
    ];
    let mut clock = Clock::new();
    for (id, &deadline) in deadlines.iter().enumerate() {
      clock.schedule_at(id as TimerId, deadline);
    }
    let mut sorted: Vec<(u64, TimerId)> = deadlines
      .iter()
      .enumerate()
      .map(|(id, &deadline)| (deadline, id as TimerId))
      .collect();
    sorted.sort();
    for (deadline, id) in sorted {
      clock.advance_to(deadline - 1);
      assert!(collect_expired(&mut clock).is_empty(), "{id} fired early");
      clock.advance_to(deadline);
      assert_eq!(collect_expired(&mut clock), vec![id], "at {deadline}");
    }
  }

  /// Each level counts exactly the entries its slots hold.
  fn assert_wheel_counts(clock: &Clock) {
    fn held<const N: usize>(wheel: &Wheel<N>) -> usize {
      wheel.slots.iter().map(Vec::len).sum()
    }
    let counted =
      [clock.level0.len, clock.level1.len, clock.level2.len, clock.level3.len];
    let held = [
      held(&clock.level0),
      held(&clock.level1),
      held(&clock.level2),
      held(&clock.level3),
    ];
    assert_eq!(counted, held, "wheel entry counts");
  }

  #[test]
  fn a_drained_slot_keeps_its_allocation() {
    let mut clock = Clock::new();
    // Due at tick 300: level 1, slot 1, until tick 256 moves it down.
    clock.schedule(1, Duration::from_millis(300 * TICK_MS));
    clock.advance_to(256);
    assert_wheel_counts(&clock);
    assert!(clock.level1.slots[1].is_empty());
    assert!(clock.level1.slots[1].capacity() > 0, "reused, not freed");
    clock.advance_to(300);
    assert_wheel_counts(&clock);
    assert_eq!(collect_expired(&mut clock), vec![1]);
  }

  #[test]
  fn idle_gap_is_skipped_not_stepped() {
    let mut clock = Clock::new();
    let day = 24 * 60 * 60 * 1000 / super::TICK_MS;
    let start = std::time::Instant::now();
    clock.advance_to(day);
    clock.schedule(1, Duration::from_millis(5));
    clock.advance_to(day * 2);
    assert_eq!(collect_expired(&mut clock), vec![1]);
    // Miri runs every step thousands of times slower, so no time bound
    // tells a skipped gap from a stepped one there.
    if !cfg!(miri) {
      assert!(start.elapsed() < Duration::from_millis(100));
    }
  }

  /// Wheel entries index timer slots unchecked, so asking for fewer slots
  /// must never drop any.
  #[test]
  fn timer_slots_never_shrink() {
    let mut clock = Clock::new();
    clock.schedule(9, Duration::from_millis(5));
    let len = clock.timers.len();
    assert!(len >= 10);
    clock.grow_timer_slots(1);
    assert_eq!(clock.timers.len(), len);
    clock.advance_by(5);
    assert_eq!(collect_expired(&mut clock), vec![9]);
  }

  /// Cancelled long timers must not pile up in the wheel until their
  /// deadline; the ones still scheduled keep firing on time.
  #[test]
  fn cancelled_timers_do_not_accumulate_in_the_wheel() {
    let mut clock = Clock::new();
    let entries = |clock: &Clock| {
      clock.level0.len + clock.level1.len + clock.level2.len + clock.level3.len
    };
    // Twice the tracked timers plus the sweep's floor, and one entry that
    // may have been inserted after the last sweep.
    let bound = |timers: usize| 2 * timers + 4 * super::LEVEL_SIZE + 1;
    // One long timer that stays, behind many that are cancelled.
    clock.schedule(0, Duration::from_secs(600));
    // Under Miri, fewer rounds: still well past what forces a sweep.
    for round in 0..if cfg!(miri) { 3 } else { 20u64 } {
      for slot in 1..1000u64 {
        let id = (round << 32) | slot;
        clock.schedule(id, Duration::from_secs(600));
        assert!(clock.remove(id));
      }
    }
    assert!(entries(&clock) <= bound(1), "{}", entries(&clock));

    // Rescheduling leaves a stale entry behind too.
    for n in 0..if cfg!(miri) { 1500 } else { 5000u64 } {
      clock.schedule(1, Duration::from_secs(600 + n));
    }
    assert!(entries(&clock) <= bound(2), "{}", entries(&clock));

    clock.advance_to(600_000 - 1);
    assert!(collect_expired(&mut clock).is_empty());
    clock.advance_to(600_000);
    assert_eq!(collect_expired(&mut clock), vec![0]);
  }

  #[test]
  #[should_panic(expected = "which none had")]
  fn leaving_an_uncounted_deadline_panics() {
    let mut clock = Clock::new();
    clock.schedule_at(1, 5);
    clock.decrement_active_deadline(9);
  }

  #[test]
  #[should_panic(expected = "which only 1 had")]
  fn leaving_a_deadline_more_often_than_counted_panics() {
    let mut clock = Clock::new();
    clock.schedule_at(1, 5);
    clock.decrement_active_deadline_by(5, 2);
  }

  #[test]
  fn deadline_counts_survive_mixed_operations() {
    let mut clock = Clock::with_capacity(16);
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = move || {
      state ^= state << 13;
      state ^= state >> 7;
      state ^= state << 17;
      state
    };
    // Every step goes through the asserting bookkeeping.
    for _ in 0..if cfg!(miri) { 300 } else { 20_000 } {
      let roll = next();
      let id = (next() % 4) << 32 | (next() % 16);
      match roll % 5 {
        0 | 1 => clock.schedule_at(id, clock.current_tick() + next() % 70_000),
        2 => {
          clock.remove(id);
        }
        3 => clock.advance_by(next() % 300),
        _ => {
          for id in collect_expired(&mut clock) {
            if next() % 2 == 0 {
              clock.remove(id);
            }
          }
        }
      }
    }
  }

  #[test]
  #[should_panic(
    expected = "did not fire by tick 10: their wheel entry is lost"
  )]
  fn a_timer_lost_from_the_wheel_panics_once_due() {
    let mut clock = Clock::new();
    clock.schedule_at(1, 5);
    clock.level0.clear_slot(5);
    clock.advance_to(10);
  }

  #[test]
  fn stale_wheel_entries_are_swept_just_past_their_bound() {
    fn entries(clock: &Clock) -> usize {
      clock.level0.len + clock.level1.len + clock.level2.len + clock.level3.len
    }
    let mut clock = Clock::new();
    let live = 100;
    // Live timers on level 1 and removed ones on level 0, so every level's
    // entries count.
    for id in 0..live {
      clock.schedule_at(id, 1_000);
    }
    // Removed timers leave their entries until they outnumber twice the
    // live timers plus a floor of four levels' slots.
    let bound = 2 * live as usize + 4 * super::LEVEL_SIZE;
    let mut id = live;
    for _ in live as usize..bound {
      clock.schedule_at(id, 100);
      assert!(clock.remove(id));
      id += 1;
    }
    assert_eq!(entries(&clock), bound, "swept before the bound");
    clock.schedule_at(id, 100);
    assert!(clock.remove(id));
    assert_eq!(entries(&clock), live as usize, "not swept past the bound");
  }

  #[test]
  fn a_timer_replaced_in_its_slot_fires_as_the_newer_one() {
    let (older, newer) = (1, (1 << 32) | 1);
    let mut clock = Clock::new();
    // Same slot and deadline, on level 1: only the generation differs.
    clock.schedule_at(older, 512);
    clock.schedule_at(newer, 512);
    clock.advance_to(512);
    assert_eq!(collect_expired(&mut clock), vec![newer]);
  }

  #[test]
  fn a_rescheduled_timer_does_not_fire_at_its_old_deadline() {
    let mut clock = Clock::new();
    // Level 1, due exactly when its slot comes round at tick 512.
    clock.schedule_at(1, 512);
    clock.schedule_at(1, 2_000);
    clock.advance_to(512);
    assert!(collect_expired(&mut clock).is_empty(), "fired at 512");
    clock.advance_to(2_000);
    assert_eq!(collect_expired(&mut clock), vec![1]);
  }

  #[test]
  #[should_panic(expected = "which is not marked stale")]
  fn delivering_an_unmarked_stale_timer_panics() {
    let mut clock = Clock::new();
    clock.schedule_at(1, 5);
    clock.advance_to(5);
    // Rescheduled after firing and before delivery: the fired entry is
    // stale, but the flag saying so is lost.
    clock.schedule_at(1, 50);
    clock.pending_has_stale = false;
    collect_expired(&mut clock);
  }

  #[test]
  fn timers_already_delivered_leave_delivery_on_its_fast_path() {
    let mut clock = Clock::new();
    clock.schedule_at(1, 5);
    clock.schedule_at(2, 5);
    clock.advance_to(5);
    assert_eq!(collect_expired(&mut clock), vec![1, 2]);
    // Delivered, then removed or rescheduled: nothing pending goes stale.
    assert!(clock.remove(1));
    clock.schedule_at(2, 50);
    assert!(!clock.pending_has_stale);
  }

  /// Random step sequences against a reference model of the wheel.
  mod model {
    use super::*;
    use proptest::prelude::*;
    use std::collections::HashMap;

    #[derive(Clone, Debug)]
    enum Step {
      Schedule {
        slot: u32,
        generation: u32,
        delay: u64,
      },
      Remove {
        slot: u32,
        generation: u32,
      },
      Advance(u64),
      /// Takes at most this many expired timers, or all of them.
      Poll(Option<usize>),
    }

    /// Delays and advances spanning every wheel level.
    fn ticks() -> impl Strategy<Value = u64> {
      prop_oneof![
        4 => 0..300u64,
        2 => 0..70_000u64,
        1 => 0..(1u64 << 26),
      ]
    }

    fn step() -> impl Strategy<Value = Step> {
      prop_oneof![
        4 => (0..8u32, 0..3u32, ticks()).prop_map(|(slot, generation, delay)| {
          Step::Schedule { slot, generation, delay }
        }),
        2 => (0..8u32, 0..3u32)
          .prop_map(|(slot, generation)| Step::Remove { slot, generation }),
        3 => ticks().prop_map(Step::Advance),
        2 => prop::option::of(0..4usize).prop_map(Step::Poll),
      ]
    }

    fn id(slot: u32, generation: u32) -> TimerId {
      (u64::from(generation) << 32) | u64::from(slot)
    }

    #[derive(Clone, Copy, Debug)]
    struct Timer {
      id: TimerId,
      deadline: u64,
      delivered: bool,
    }

    fn run(steps: Vec<Step>) {
      // Smaller than the slots used, so the timer table grows.
      let mut clock = Clock::with_capacity(4);
      let mut timers: HashMap<u32, Timer> = HashMap::new();
      let mut now = 0u64;

      for step in steps {
        match step {
          Step::Schedule { slot, generation, delay } => {
            let id = id(slot, generation);
            clock.schedule_at(id, now + delay);
            // A newer timer in the same slot replaces the older one.
            let deadline = (now + delay).max(now + 1);
            timers.insert(slot, Timer { id, deadline, delivered: false });
          }
          Step::Remove { slot, generation } => {
            let id = id(slot, generation);
            let tracked = timers.get(&slot).is_some_and(|t| t.id == id);
            assert_eq!(clock.remove(id), tracked, "remove({id:#x})");
            if tracked {
              timers.remove(&slot);
            }
          }
          Step::Advance(by) => {
            now += by;
            clock.advance_by(by);
            assert_eq!(clock.current_tick(), now);
          }
          Step::Poll(limit) => {
            let got: Vec<TimerId> = match limit {
              Some(limit) => clock.poll_expired().take(limit).collect(),
              // `for_each` takes the iterator's `fold`, as the driver does;
              // bounded, so a broken one fails rather than runs forever.
              None => {
                let mut got = Vec::new();
                let tracked = clock.len();
                crate::time::at_most(tracked, clock.poll_expired())
                  .for_each(|id| got.push(id));
                got
              }
            };
            for &id in &got {
              let timer = timers
                .get_mut(&(id as u32))
                .filter(|t| t.id == id)
                .unwrap_or_else(|| panic!("delivered untracked timer {id:#x}"));
              assert!(timer.deadline <= now, "{id:#x} delivered early");
              assert!(!timer.delivered, "{id:#x} delivered twice");
              timer.delivered = true;
            }
            let latest = got
              .iter()
              .map(|id| timers[&(*id as u32)].deadline)
              .max()
              .unwrap_or(0);
            let due: Vec<_> = timers
              .values()
              .filter(|t| t.deadline <= now && !t.delivered)
              .collect();
            for timer in &due {
              assert!(
                timer.deadline >= latest,
                "{:#x} due at {} still pending after one due at {latest}",
                timer.id,
                timer.deadline
              );
            }
            if limit.is_none() {
              assert!(
                due.is_empty(),
                "polling everything leaves none: {due:?}"
              );
            }
          }
        }

        assert_wheel_counts(&clock);
        assert_eq!(clock.is_empty(), timers.is_empty());
        for timer in timers.values() {
          assert!(clock.contains(timer.id), "{:#x} is tracked", timer.id);
        }
        let active = timers.values().filter(|t| t.deadline > now);
        assert_eq!(
          clock.earliest_active_deadline(),
          active.map(|t| t.deadline).min()
        );
        if timers.values().any(|t| t.deadline <= now && !t.delivered) {
          assert!(clock.has_expired(), "an undelivered timer is due");
        }
      }
    }

    /// Miri runs each step thousands of times slower.
    const STEPS: usize = if cfg!(miri) { 16 } else { 120 };

    proptest! {
      #[test]
      fn clock_matches_model(
        steps in prop::collection::vec(step(), 0..STEPS),
      ) {
        run(steps);
      }
    }
  }
}
