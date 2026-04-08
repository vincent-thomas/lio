mod clock;

use crate::{Lio, time::{self, TimeManager}};
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
