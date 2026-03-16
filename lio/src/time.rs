//! Time management for lio.
//!
//! This module provides efficient timer management using a timing wheel.
//! It's used internally by `Lio` to handle timeout operations without
//! creating a kernel timer per operation.
//!
//! # Design
//!
//! - Timers are stored in a userspace min-heap sorted by deadline
//! - `Lio` uses `next_deadline()` to compute the wait timeout for the kernel
//! - When the kernel returns, `poll_expired()` collects all fired timers
//! - Supports pause/resume/cancel for future use cases
//!
//! # Thread Safety
//!
//! `TimeManager` is `!Send` and `!Sync` - it's designed for single-threaded,
//! thread-per-core usage matching lio's design.

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};
use std::marker::PhantomData;
use std::time::{Duration, Instant};

/// Unique identifier for a timer (matches operation ID).
pub type TimerId = u64;

/// State of a timer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimerState {
  /// Timer is active and will fire at its deadline.
  Active,
  /// Timer is paused with remaining duration preserved.
  Paused,
  /// Timer has been cancelled.
  Cancelled,
  /// Timer has fired.
  Fired,
}

/// A timer entry.
#[derive(Debug)]
struct TimerEntry {
  /// Absolute deadline when this timer should fire.
  deadline: Instant,
  /// Current state.
  state: TimerState,
  /// Remaining duration when paused.
  remaining: Option<Duration>,
}

/// Wrapper for BinaryHeap that orders by earliest deadline first.
#[derive(Debug)]
struct HeapEntry {
  deadline: Instant,
  id: TimerId,
}

impl PartialEq for HeapEntry {
  fn eq(&self, other: &Self) -> bool {
    self.deadline == other.deadline && self.id == other.id
  }
}

impl Eq for HeapEntry {}

impl PartialOrd for HeapEntry {
  fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
    Some(self.cmp(other))
  }
}

impl Ord for HeapEntry {
  fn cmp(&self, other: &Self) -> Ordering {
    // Reverse ordering for min-heap (earliest deadline first)
    other.deadline.cmp(&self.deadline)
      .then_with(|| other.id.cmp(&self.id))
  }
}

/// Manages all timer state for lio.
///
/// This struct is used internally by `Lio` to efficiently handle timeout
/// operations using a single kernel timer (via the wait timeout) instead
/// of one kernel resource per timer.
///
/// # Example
///
/// ```rust
/// use lio::time::TimeManager;
/// use std::time::Duration;
///
/// let mut time = TimeManager::new();
///
/// // Schedule a timer
/// time.schedule(42, Duration::from_millis(100));
///
/// // Get next deadline for kernel wait
/// if let Some(wait_time) = time.next_deadline() {
///     // Pass to backend.wait_timeout(Some(wait_time))
/// }
///
/// // After kernel returns, check for expired timers
/// for timer_id in time.poll_expired() {
///     println!("Timer {} fired", timer_id);
/// }
/// ```
#[derive(Debug)]
pub struct TimeManager {
  /// Min-heap of timer deadlines.
  heap: BinaryHeap<HeapEntry>,
  /// Map of timer ID to entry for O(1) lookup.
  timers: HashMap<TimerId, TimerEntry>,
  /// Marker to make this type `!Send`
  _not_send: PhantomData<*const ()>,
}

impl Default for TimeManager {
  fn default() -> Self {
    Self::new()
  }
}

impl TimeManager {
  /// Creates a new time manager.
  pub fn new() -> Self {
    Self {
      heap: BinaryHeap::new(),
      timers: HashMap::new(),
      _not_send: PhantomData,
    }
  }

  /// Creates a time manager with pre-allocated capacity.
  pub fn with_capacity(capacity: usize) -> Self {
    Self {
      heap: BinaryHeap::with_capacity(capacity),
      timers: HashMap::with_capacity(capacity),
      _not_send: PhantomData,
    }
  }

  /// Schedules a timer with the given ID and duration.
  ///
  /// The ID should match the operation ID used by lio.
  pub fn schedule(&mut self, id: TimerId, duration: Duration) {
    let now = Instant::now();
    let deadline = now + duration;

    let entry = TimerEntry {
      deadline,
      state: TimerState::Active,
      remaining: None,
    };

    self.timers.insert(id, entry);
    self.heap.push(HeapEntry { deadline, id });
  }

  /// Pauses a timer, preserving its remaining duration.
  ///
  /// Returns `true` if the timer was active and is now paused.
  pub fn pause(&mut self, id: TimerId) -> bool {
    if let Some(entry) = self.timers.get_mut(&id) {
      if entry.state == TimerState::Active {
        let now = Instant::now();
        let remaining = if entry.deadline > now {
          entry.deadline - now
        } else {
          Duration::ZERO
        };
        entry.remaining = Some(remaining);
        entry.state = TimerState::Paused;
        return true;
      }
    }
    false
  }

  /// Resumes a paused timer.
  ///
  /// Returns `true` if the timer was paused and is now active.
  pub fn resume(&mut self, id: TimerId) -> bool {
    if let Some(entry) = self.timers.get_mut(&id) {
      if entry.state == TimerState::Paused {
        if let Some(remaining) = entry.remaining.take() {
          let now = Instant::now();
          entry.deadline = now + remaining;
          entry.state = TimerState::Active;
          // Re-add to heap with new deadline
          self.heap.push(HeapEntry { deadline: entry.deadline, id });
          return true;
        }
      }
    }
    false
  }

  /// Cancels a timer.
  ///
  /// Returns `true` if the timer existed and was cancelled.
  pub fn cancel(&mut self, id: TimerId) -> bool {
    if let Some(entry) = self.timers.get_mut(&id) {
      if entry.state != TimerState::Fired && entry.state != TimerState::Cancelled {
        entry.state = TimerState::Cancelled;
        return true;
      }
    }
    false
  }

  /// Gets the state of a timer.
  pub fn state(&self, id: TimerId) -> Option<TimerState> {
    self.timers.get(&id).map(|e| e.state)
  }

  /// Returns the duration until the next timer fires, if any.
  ///
  /// Use this to compute the timeout for the kernel wait call:
  /// ```rust,ignore
  /// let wait_timeout = match (user_timeout, time.next_deadline()) {
  ///     (Some(u), Some(t)) => Some(u.min(t)),
  ///     (Some(u), None) => Some(u),
  ///     (None, Some(t)) => Some(t),
  ///     (None, None) => None,
  /// };
  /// backend.wait_timeout(wait_timeout)?;
  /// ```
  pub fn next_deadline(&mut self) -> Option<Duration> {
    self.clean_heap();

    self.heap.peek().map(|entry| {
      let now = Instant::now();
      if entry.deadline <= now {
        Duration::ZERO
      } else {
        entry.deadline - now
      }
    })
  }

  /// Returns `true` if there are active timers.
  pub fn has_pending(&self) -> bool {
    self.timers.values().any(|t| t.state == TimerState::Active)
  }

  /// Returns the number of active timers.
  pub fn pending_count(&self) -> usize {
    self.timers.values().filter(|t| t.state == TimerState::Active).count()
  }

  /// Polls for expired timers.
  ///
  /// Returns an iterator over timer IDs that have fired.
  /// Call this after the kernel wait returns.
  pub fn poll_expired(&mut self) -> impl Iterator<Item = TimerId> + '_ {
    ExpiredIterator { manager: self }
  }

  /// Removes a timer from tracking.
  ///
  /// Call this after completing the timer operation.
  pub fn remove(&mut self, id: TimerId) -> bool {
    self.timers.remove(&id).is_some()
  }

  /// Cleans up stale entries from the heap.
  fn clean_heap(&mut self) {
    while let Some(entry) = self.heap.peek() {
      let should_remove = match self.timers.get(&entry.id) {
        None => true, // Timer was removed
        Some(timer) => {
          timer.state != TimerState::Active || timer.deadline != entry.deadline
        }
      };

      if should_remove {
        self.heap.pop();
      } else {
        break;
      }
    }
  }

  /// Pops the next expired timer, if any.
  fn pop_expired(&mut self) -> Option<TimerId> {
    let now = Instant::now();

    loop {
      let entry = self.heap.peek()?;

      // Check if the timer entry is still valid
      let Some(timer) = self.timers.get(&entry.id) else {
        self.heap.pop();
        continue;
      };

      // Skip cancelled or paused timers
      if timer.state != TimerState::Active {
        self.heap.pop();
        continue;
      }

      // Check if deadline matches (timer might have been rescheduled)
      if timer.deadline != entry.deadline {
        self.heap.pop();
        continue;
      }

      // Check if timer has expired
      if entry.deadline > now {
        return None; // No more expired timers
      }

      // Timer has fired
      let heap_entry = self.heap.pop().unwrap();
      if let Some(timer) = self.timers.get_mut(&heap_entry.id) {
        timer.state = TimerState::Fired;
        return Some(heap_entry.id);
      }
    }
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
    assert_eq!(time.state(1), Some(TimerState::Active));
    assert!(time.has_pending());

    thread::sleep(Duration::from_millis(5));

    let expired: Vec<_> = time.poll_expired().collect();
    assert_eq!(expired, vec![1]);
    assert_eq!(time.state(1), Some(TimerState::Fired));
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

    time.schedule(1, Duration::from_millis(100));

    assert!(time.pause(1));
    assert_eq!(time.state(1), Some(TimerState::Paused));

    thread::sleep(Duration::from_millis(20));

    assert!(time.resume(1));
    assert_eq!(time.state(1), Some(TimerState::Active));

    // Should not have fired yet
    let expired: Vec<_> = time.poll_expired().collect();
    assert!(expired.is_empty());
  }

  #[test]
  fn test_cancel() {
    let mut time = TimeManager::new();

    time.schedule(1, Duration::from_millis(100));

    assert!(time.cancel(1));
    assert_eq!(time.state(1), Some(TimerState::Cancelled));

    thread::sleep(Duration::from_millis(150));

    let expired: Vec<_> = time.poll_expired().collect();
    assert!(expired.is_empty());
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
    assert_eq!(time.state(1), None);
  }

  #[test]
  fn test_pending_count() {
    let mut time = TimeManager::new();

    assert_eq!(time.pending_count(), 0);

    time.schedule(1, Duration::from_millis(100));
    time.schedule(2, Duration::from_millis(100));
    assert_eq!(time.pending_count(), 2);

    time.cancel(1);
    assert_eq!(time.pending_count(), 1);
  }
}
