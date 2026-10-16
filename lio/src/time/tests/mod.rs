mod clock;

use crate::{
  Lio,
  time::{self, TimeManager},
};
use std::{
  thread,
  time::{Duration, Instant},
};

#[test]
fn a_poll_may_fire_every_tracked_timer() {
  assert_eq!(time::at_most(2, [1, 2].into_iter()).count(), 2);
}

#[test]
#[should_panic(expected = "fired more timers than the 2 tracked")]
fn a_poll_firing_more_timers_than_tracked_panics() {
  time::at_most(2, std::iter::repeat(7)).for_each(drop);
}

#[test]
fn ticks_span_whole_ticks() {
  let tick = Duration::from_millis(time::clock::TICK_MS);
  assert_eq!(time::clock::ticks_duration(0), Duration::ZERO);
  assert_eq!(time::clock::ticks_duration(5), tick * 5);
}

#[test]
fn a_running_clock_reads_the_whole_ticks_elapsed() {
  let time = TimeManager::new();
  thread::sleep(Duration::from_millis(2));
  let time::TimeState::Running { epoch } = time.state else {
    unreachable!("a new clock runs");
  };
  let ticks = time.now_ticks();
  let tick = Duration::from_millis(time::clock::TICK_MS);
  assert!(ticks >= 2 / time::clock::TICK_MS, "{ticks}");
  assert!(ticks as u128 <= epoch.elapsed().as_nanos() / tick.as_nanos());
}

#[test]
fn long_deadlines_count_whole_ticks() {
  let long = Duration::from_secs(1_000_000);
  let ticks = long.as_millis() as u64 / time::clock::TICK_MS;

  let mut paused = TimeManager::new();
  paused.pause();
  let now = paused.clock.current_tick();
  paused.schedule(1, long);
  assert_eq!(paused.clock.earliest_active_deadline(), Some(now + ticks));

  // Running, the deadline also counts the time since the clock started,
  // under a second here.
  let mut running = TimeManager::new();
  running.schedule(1, long);
  let deadline = running.clock.earliest_active_deadline().unwrap();
  assert!((ticks..ticks + 1_000).contains(&deadline), "{deadline}");
}

#[test]
fn advancing_a_running_clock_moves_its_reading() {
  let mut time = TimeManager::new();
  time.advance_by_ticks(5_000);
  assert!(time.now_ticks() >= 5_000);

  let mut time = TimeManager::new();
  time.pause();
  time.advance_by_ticks(5_000);
  time.resume();
  assert!(time.now_ticks() >= 5_000, "resuming keeps the advanced time");
}

#[test]
fn only_removed_timers_waiting_to_drain_do_not_shorten_the_wait() {
  let mut time = TimeManager::new();
  time.pause();
  time.schedule(1, Duration::from_millis(1));
  time.advance_by_ticks(5);
  // Fired, then removed before the driver drained it: nothing is due.
  assert!(time.remove(1));
  let second = Duration::from_secs(1);
  assert_eq!(time.wait_timeout(Some(second)), Some(second));
  assert_eq!(time.poll_expired().count(), 0);
}

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
  // Long enough that no scheduler delay makes it due during the test, so
  // the bounds below come from measured time rather than sleep accuracy.
  const TIMER: Duration = Duration::from_secs(10);
  const GAP: Duration = Duration::from_millis(300);
  // Deadlines round up to a tick and the clock reads down to one.
  let slack = Duration::from_millis(2);

  let mut time = TimeManager::new();
  let started = Instant::now();
  time.schedule(1, TIMER);
  thread::sleep(Duration::from_millis(50));
  time.pause();
  let ran_before = started.elapsed();

  thread::sleep(GAP);
  assert!(time.poll_expired().next().is_none());
  assert_eq!(time.next_deadline(), None, "paused time never comes due");

  let resumed = Instant::now();
  time.resume();
  let remaining = time.next_deadline().expect("the timer is still pending");
  let ran = ran_before + resumed.elapsed();

  // Only the time the clock ran counts against the timer, never the gap.
  assert!(
    remaining + ran + slack >= TIMER,
    "{remaining:?} left after running {ran:?}: the paused gap was counted"
  );
  assert!(
    remaining <= TIMER - Duration::from_millis(50) + slack,
    "{remaining:?} left: the time before the pause was not counted"
  );
  assert!(time.poll_expired().next().is_none());
}
