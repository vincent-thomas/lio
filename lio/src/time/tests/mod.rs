mod clock;

use crate::{
  Lio,
  time::{self, TimeManager},
};
use std::{thread, time::Duration};

#[test]
fn wall_clock_adapter_smoke() {
  let mut time = TimeManager::new();
  time.schedule(1, Duration::from_millis(1));

  thread::sleep(Duration::from_millis(5));

  let expired: Vec<_> = time.poll_expired().collect();
  assert_eq!(expired, vec![1]);
}

#[test]
fn pause_and_resume_freezes_wall_clock_progress() {
  let mut time = TimeManager::new();

  time.schedule(1, Duration::from_millis(10));
  time.pause();
  assert!(time.is_paused());

  thread::sleep(Duration::from_millis(20));
  assert!(time.poll_expired().collect::<Vec<_>>().is_empty());

  time.resume();
  assert!(!time.is_paused());
}

#[test]
fn paused_time_gives_no_wait_deadline() {
  let mut time = TimeManager::new();
  time.schedule(1, Duration::from_millis(10));
  assert!(time.next_deadline().is_some());

  // Waiting cannot make a paused timer due, so the driver must not wake up
  // for it.
  time.pause();
  assert_eq!(time.next_deadline(), None);

  time.resume();
  assert!(time.next_deadline().is_some());
}

#[test]
fn wait_timeout_is_bounded_by_the_next_timer() {
  let mut time = TimeManager::new();
  let second = Duration::from_secs(1);
  // No timers: the caller's timeout, whatever it is.
  assert_eq!(time.wait_timeout(None), None);
  assert_eq!(time.wait_timeout(Some(second)), Some(second));

  // With one: at most the time to its deadline (rounded up to a whole tick,
  // so it never fires early), or less if asked.
  time.schedule(1, Duration::from_millis(10));
  let limit = Duration::from_millis(11);
  assert!(time.wait_timeout(None).is_some_and(|wait| wait <= limit));
  assert!(time.wait_timeout(Some(second)).is_some_and(|wait| wait <= limit));
  assert_eq!(time.wait_timeout(Some(Duration::ZERO)), Some(Duration::ZERO));
}

#[test]
fn public_pause_resume_helpers_toggle_lio_time() {
  let lio = Lio::new(8).unwrap();
  time::pause(&lio);
  time::resume(&lio);
}

#[test]
fn pause_preserves_remaining_sleep_time_across_real_time_gap() {
  let mut time = TimeManager::new();
  time.schedule(1, Duration::from_millis(200));

  thread::sleep(Duration::from_millis(100));
  time.pause();

  thread::sleep(Duration::from_millis(500));
  assert!(time.poll_expired().collect::<Vec<_>>().is_empty());

  time.resume();
  assert!(time.poll_expired().collect::<Vec<_>>().is_empty());

  thread::sleep(Duration::from_millis(120));
  let expired: Vec<_> = time.poll_expired().collect();
  assert_eq!(expired, vec![1]);
}
