//! Exercises calls that block: timers from [`lio_wasi::clock::WaitFor`].
//!
//! ```text
//! cargo build -p lio-wasi --example timer --target wasm32-wasip3
//! wasmtime run -S p3 target/wasm32-wasip3/debug/examples/timer.wasm
//! ```

use std::time::{Duration, Instant};

use lio_wasi::clock::WaitFor;
use lio_wasi::{Cancel, Event, Started, StreamResult, Subtask, WaitableSet};

/// Starts a timer that cannot have fired yet.
///
/// # Safety
///
/// As for [`WaitableSet::start`].
unsafe fn start(set: &mut WaitableSet, timer: &mut WaitFor) -> Subtask {
  // SAFETY: guaranteed by the caller.
  let Some(subtask) = (unsafe { set.start(timer) }) else {
    panic!("a timer fired as it was started");
  };
  subtask
}

fn main() {
  let mut set = WaitableSet::new();

  // Three timers started longest first fire shortest first.
  let mut timers = [
    WaitFor::new(Duration::from_millis(300)),
    WaitFor::new(Duration::from_millis(200)),
    WaitFor::new(Duration::from_millis(100)),
  ];
  let start_time = Instant::now();
  // SAFETY: `timers` stays put until every timer has fired.
  let subtasks: Vec<Subtask> =
    timers.iter_mut().map(|timer| unsafe { start(&mut set, timer) }).collect();
  assert!(set.poll().is_none(), "no timer has fired yet");
  assert_eq!(set.in_flight(), 3);
  let mut order = Vec::new();
  while let Some(event) = set.wait() {
    let Event::Returned(subtask) = event else {
      unreachable!("only timers are in flight: {event:?}")
    };
    order.push(subtasks.iter().position(|started| *started == subtask));
  }
  assert_eq!(order, [Some(2), Some(1), Some(0)]);
  let elapsed = start_time.elapsed();
  assert!(elapsed >= Duration::from_millis(300), "fired early: {elapsed:?}");
  assert!(elapsed < Duration::from_millis(900), "fired late: {elapsed:?}");
  println!("three timers fired in order after {elapsed:?}");

  // A cancelled timer is stopped at once, and never fires.
  let mut long = WaitFor::new(Duration::from_secs(3600));
  let start_time = Instant::now();
  // SAFETY: `long` stays put until the timer is cancelled.
  let subtask = unsafe { start(&mut set, &mut long) };
  assert_eq!(set.cancel(subtask), Some(Cancel::Cancelled));
  assert_eq!(set.cancel(subtask), None);
  assert!(set.wait().is_none());
  assert!(start_time.elapsed() < Duration::from_secs(1));
  println!("cancelled timer: ok");

  // A timer bounds a wait on a stream that never has data: the lio
  // backend's `wait(timeout)`.
  let (_writer, reader) = wasip3::wit_stream::new::<u8>();
  let reader = set.add_reader(reader);
  let mut buf = [0u8; 8];
  let mut timeout = WaitFor::new(Duration::from_millis(50));
  // SAFETY: `buf` and `timeout` outlive the read and the timer, which have
  // both ended by the cancel below.
  let (read, timer) = unsafe {
    (
      set.read(reader, buf.as_mut_ptr(), buf.len()),
      start(&mut set, &mut timeout),
    )
  };
  assert_eq!(read, Ok(Started::Blocked));
  assert_eq!(set.wait(), Some(Event::Returned(timer)), "the timeout is first");
  assert_eq!(set.cancel_read(reader), Some(StreamResult::Cancelled(0)));
  assert!(set.wait().is_none());
  println!("timeout bounds a wait: ok");

  // Dropping the set with a timer in flight cancels it.
  let mut pending = WaitFor::new(Duration::from_secs(3600));
  // SAFETY: the set is dropped before `pending`.
  let _ = unsafe { start(&mut set, &mut pending) };
  drop(set);
  println!("drop with a timer in flight: ok");
}
