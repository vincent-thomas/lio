//! Exercises calls that block: timers from [`lio_wasi::clock::WaitFor`].
//!
//! ```text
//! cargo build -p lio-wasi --example timer --target wasm32-wasip3
//! wasmtime run -S p3 target/wasm32-wasip3/debug/examples/timer.wasm
//! ```

use std::time::{Duration, Instant};

use lio_wasi::clock::WaitFor;
use lio_wasi::{Completion, Outcome, Queue};

fn next(queue: &mut Queue) -> Completion {
  let Some(done) = queue.wait() else {
    unreachable!("an operation is outstanding")
  };
  done
}

fn main() {
  let mut queue = Queue::new();

  // Three timers pushed longest first complete shortest first.
  let mut timers = [
    WaitFor::new(Duration::from_millis(300)),
    WaitFor::new(Duration::from_millis(200)),
    WaitFor::new(Duration::from_millis(100)),
  ];
  let start = Instant::now();
  for (id, timer) in timers.iter_mut().enumerate() {
    // SAFETY: `timers` stays put until every completion is taken.
    unsafe { queue.push_call(timer, id as u64) };
  }
  assert!(queue.poll().is_none(), "no timer has fired yet");
  assert_eq!(queue.outstanding(), 3);
  let order: Vec<u64> = (0..3)
    .map(|_| {
      let done = next(&mut queue);
      assert_eq!(done.outcome(), Outcome::Returned);
      done.user_data()
    })
    .collect();
  assert_eq!(order, [2, 1, 0]);
  let elapsed = start.elapsed();
  assert!(elapsed >= Duration::from_millis(300), "fired early: {elapsed:?}");
  assert!(elapsed < Duration::from_millis(900), "fired late: {elapsed:?}");
  println!("three timers fired in order after {elapsed:?}");

  // A cancelled timer completes at once as cancelled, and never fires.
  let mut long = WaitFor::new(Duration::from_secs(3600));
  let start = Instant::now();
  // SAFETY: `long` stays put until its completion is taken.
  unsafe { queue.push_call(&mut long, 7) };
  assert!(queue.cancel(7));
  assert!(!queue.cancel(7));
  let done = next(&mut queue);
  assert_eq!((done.user_data(), done.outcome()), (7, Outcome::Cancelled(0)));
  assert!(queue.wait().is_none());
  assert!(start.elapsed() < Duration::from_secs(1));
  println!("cancelled timer: ok");

  // A timer bounds a wait on a stream that never has data: the lio
  // backend's `wait(timeout)`.
  let (_writer, reader) = wasip3::wit_stream::new::<u8>();
  let reader = queue.add_reader(reader);
  let mut buf = [0u8; 8];
  let mut timeout = WaitFor::new(Duration::from_millis(50));
  // SAFETY: `buf` and `timeout` outlive the completions taken below.
  unsafe {
    assert_eq!(queue.push_read(reader, buf.as_mut_ptr(), buf.len(), 1), Ok(()));
    queue.push_call(&mut timeout, 2);
  }
  assert_eq!(next(&mut queue).user_data(), 2, "the timeout comes first");
  assert!(queue.cancel(1));
  assert_eq!(next(&mut queue).outcome(), Outcome::Cancelled(0));
  println!("timeout bounds a wait: ok");

  // Dropping the queue with a timer in flight cancels it.
  let mut pending = WaitFor::new(Duration::from_secs(3600));
  // SAFETY: the queue is dropped before `pending`.
  unsafe { queue.push_call(&mut pending, 3) };
  drop(queue);
  println!("drop with a timer in flight: ok");
}
