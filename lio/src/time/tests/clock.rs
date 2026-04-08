use crate::time::{Clock, TimerId};
use std::time::Duration;

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
