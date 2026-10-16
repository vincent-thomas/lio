//! User code that runs on completion (callbacks and task wake-ups) must be
//! free to use the same `Lio`: schedule follow-ups, drop operations, or drive
//! the loop.

use std::cell::{Cell, RefCell};
use std::future::{Future, IntoFuture};
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use std::time::Duration;

use lio::{Lio, api};

/// Runs the loop until `done` is set, failing after a generous deadline.
fn run_until(lio: &Lio, done: &Cell<bool>) {
  for _ in 0..1000 {
    if done.get() {
      return;
    }
    lio.run_timeout(Duration::from_millis(5)).unwrap();
  }
  panic!("operation never completed");
}

#[test]
fn timer_callback_schedules_a_follow_up() {
  let lio = Lio::new(8).unwrap();
  let done = Rc::new(Cell::new(false));

  let (inner_lio, inner_done) = (lio.clone(), done.clone());
  api::sleep(Duration::from_millis(1)).with_lio(&lio).when_done(move |_| {
    let done = inner_done.clone();
    api::sleep(Duration::from_millis(1))
      .with_lio(&inner_lio)
      .when_done(move |_| done.set(true));
  });

  run_until(&lio, &done);
}

#[test]
fn io_callback_schedules_a_follow_up() {
  let lio = Lio::new(8).unwrap();
  let done = Rc::new(Cell::new(false));

  let (inner_lio, inner_done) = (lio.clone(), done.clone());
  api::nop().with_lio(&lio).when_done(move |_| {
    let done = inner_done.clone();
    api::nop().with_lio(&inner_lio).when_done(move |_| done.set(true));
  });

  run_until(&lio, &done);
}

#[test]
fn callback_drives_the_loop_itself() {
  let lio = Lio::new(8).unwrap();
  let done = Rc::new(Cell::new(false));

  let (inner_lio, inner_done) = (lio.clone(), done.clone());
  api::nop().with_lio(&lio).when_done(move |_| {
    let done = inner_done.clone();
    api::nop().with_lio(&inner_lio).when_done(move |_| done.set(true));
    // A nested run completes the follow-up before this callback returns.
    while !inner_done.get() {
      inner_lio.run_timeout(Duration::from_millis(5)).unwrap();
    }
  });

  run_until(&lio, &done);
}

#[test]
fn callback_drops_an_in_flight_future() {
  let lio = Lio::new(8).unwrap();
  let done = Rc::new(Cell::new(false));

  // A future that has submitted its operation but will never be polled again.
  let pending = Rc::new(RefCell::new(Some(Box::pin(
    api::sleep(Duration::from_secs(60)).with_lio(&lio).into_future(),
  ))));
  let waker = Waker::noop();
  let mut cx = Context::from_waker(waker);
  let poll = pending.borrow_mut().as_mut().unwrap().as_mut().poll(&mut cx);
  assert!(poll.is_pending());

  let inner_done = done.clone();
  api::nop().with_lio(&lio).when_done(move |_| {
    // Dropping it cancels the operation through the same `Lio`.
    drop(pending.borrow_mut().take());
    inner_done.set(true);
  });

  run_until(&lio, &done);
}

/// A sleep future that has submitted its operation and is never polled again.
fn submitted_sleep(lio: &Lio) -> Pin<Box<impl Future + use<>>> {
  let mut sleep =
    Box::pin(api::sleep(Duration::from_secs(60)).with_lio(lio).into_future());
  let mut cx = Context::from_waker(Waker::noop());
  assert!(sleep.as_mut().poll(&mut cx).is_pending());
  sleep
}

#[test]
fn dropping_a_finished_callback_may_cancel_through_the_lio() {
  // Capacity 2: both slots must come back.
  let lio = Lio::new(2).unwrap();
  let sleep = submitted_sleep(&lio);
  let done = Rc::new(Cell::new(false));

  // The closure owns the in-flight sleep. Dropping the closure, when its own
  // operation finishes, cancels the sleep through the same `Lio`.
  let inner_done = done.clone();
  api::nop().with_lio(&lio).when_done(move |_| {
    let _ = &sleep;
    inner_done.set(true);
  });
  run_until(&lio, &done);

  assert!(!lio.is_full(), "both operations were released");
  let again = Rc::new(Cell::new(false));
  let (a, b) = (again.clone(), again.clone());
  api::nop().with_lio(&lio).when_done(move |_| a.set(true));
  api::nop().with_lio(&lio).when_done(move |_| b.set(true));
  run_until(&lio, &again);
}

#[test]
fn panicking_callback_that_owns_an_in_flight_future_does_not_abort() {
  let lio = Lio::new(2).unwrap();
  let sleep = submitted_sleep(&lio);

  api::nop().with_lio(&lio).when_done(move |_| {
    let _ = &sleep;
    panic!("callback failure");
  });
  let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
    lio.run_timeout(Duration::from_millis(50)).unwrap();
  }));
  assert!(run.is_err(), "the callback panic propagates");

  // The next run drops the panicked registration, and with it the sleep.
  lio.try_run().unwrap();
  assert!(!lio.is_full(), "both operations were released");
}

/// A waker that schedules more work on the `Lio` when woken.
struct SchedulingWaker {
  lio: Lio,
  done: Rc<Cell<bool>>,
}

// SAFETY: the test drives everything on one thread.
unsafe impl Send for SchedulingWaker {}
// SAFETY: as above.
unsafe impl Sync for SchedulingWaker {}

impl Wake for SchedulingWaker {
  fn wake(self: Arc<Self>) {
    let done = self.done.clone();
    api::nop().with_lio(&self.lio).when_done(move |_| done.set(true));
  }
}

#[test]
fn waker_schedules_work_when_woken() {
  let lio = Lio::new(8).unwrap();
  let done = Rc::new(Cell::new(false));

  let waker = Waker::from(Arc::new(SchedulingWaker {
    lio: lio.clone(),
    done: done.clone(),
  }));
  let mut cx = Context::from_waker(&waker);
  let mut future: Pin<Box<_>> =
    Box::pin(api::nop().with_lio(&lio).into_future());
  assert!(future.as_mut().poll(&mut cx).is_pending());

  run_until(&lio, &done);
  assert!(matches!(future.as_mut().poll(&mut cx), Poll::Ready(_)));
}

type NopFuture = Pin<Box<dyn Future<Output = std::io::Result<()>>>>;

/// A waker that polls its future inline when woken, as an executor running
/// tasks from `wake` would.
struct PollingWaker {
  future: RefCell<Option<NopFuture>>,
  done: Rc<Cell<bool>>,
}

// SAFETY: the test drives everything on one thread.
unsafe impl Send for PollingWaker {}
// SAFETY: as above.
unsafe impl Sync for PollingWaker {}

impl Wake for PollingWaker {
  fn wake(self: Arc<Self>) {
    let mut cx = Context::from_waker(Waker::noop());
    let mut future = self.future.borrow_mut();
    if let Some(f) = future.as_mut()
      && f.as_mut().poll(&mut cx).is_ready()
    {
      *future = None;
      self.done.set(true);
    }
  }
}

#[test]
fn waker_may_poll_its_future_inline() {
  let lio = Lio::new(8).unwrap();
  let done = Rc::new(Cell::new(false));
  let polling = Arc::new(PollingWaker {
    future: RefCell::new(Some(Box::pin(
      api::nop().with_lio(&lio).into_future(),
    ))),
    done: done.clone(),
  });

  // The first poll submits the op and stores this waker.
  let waker = Waker::from(polling.clone());
  let mut cx = Context::from_waker(&waker);
  let pending =
    polling.future.borrow_mut().as_mut().unwrap().as_mut().poll(&mut cx);
  assert!(pending.is_pending());

  run_until(&lio, &done);
}

#[test]
fn panicking_callback_frees_its_slot() {
  // Capacity 1: if the panicking registration leaked, nothing more could be
  // scheduled.
  let lio = Lio::new(1).unwrap();

  api::nop().with_lio(&lio).when_done(|_| panic!("callback failure"));
  let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
    lio.run_timeout(Duration::from_millis(50)).unwrap();
  }));
  assert!(run.is_err(), "the callback panic propagates to the caller");

  let done = Rc::new(Cell::new(false));
  let inner_done = done.clone();
  api::nop().with_lio(&lio).when_done(move |_| inner_done.set(true));
  run_until(&lio, &done);
}

const MAX_BLOCK: Duration = Duration::from_secs(2);

/// Completes every queued no-op on the next wait, in submission order.
/// With nothing queued, a wait blocks for its timeout (at most two seconds)
/// like a real backend would.
#[derive(Default)]
struct InOrderBackend {
  queued: Vec<u64>,
}

impl lio::backend::IoBackend for InOrderBackend {
  fn init(&mut self, _cap: usize) -> std::io::Result<()> {
    Ok(())
  }

  fn push(
    &mut self,
    id: u64,
    _op: lio::backend::op::Op,
    _step_bump: &mut bumpalo::Bump,
  ) {
    self.queued.push(id);
  }

  fn flush(&mut self) -> std::io::Result<()> {
    Ok(())
  }

  fn wait(
    &mut self,
    timeout: Option<Duration>,
    completed: &mut Vec<lio::backend::OpCompleted>,
  ) -> std::io::Result<()> {
    completed.clear();
    if self.queued.is_empty() {
      std::thread::sleep(timeout.unwrap_or(Duration::MAX).min(MAX_BLOCK));
    }
    completed.extend(
      self.queued.drain(..).map(|id| lio::backend::OpCompleted::new(id, 0)),
    );
    Ok(())
  }
}

#[test]
fn panic_does_not_lose_the_rest_of_the_batch() {
  let lio = Lio::new_with_backend(InOrderBackend::default(), 4).unwrap();
  let done = Rc::new(Cell::new(false));

  // Both complete in the same run, the panicking one first.
  api::nop().with_lio(&lio).when_done(|_| panic!("callback failure"));
  let inner_done = done.clone();
  api::nop().with_lio(&lio).when_done(move |_| inner_done.set(true));

  let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
    lio.try_run().unwrap();
  }));
  assert!(run.is_err());
  assert!(!done.get(), "the second callback is after the panic");

  // The next run delivers the completion the panicking run did not reach.
  lio.try_run().unwrap();
  assert!(done.get());
}

#[test]
fn blocking_run_delivers_what_a_panic_left_behind() {
  let lio = Lio::new_with_backend(InOrderBackend::default(), 4).unwrap();
  let done = Rc::new(Cell::new(false));

  // Both complete in the same run, the panicking one first.
  api::nop().with_lio(&lio).when_done(|_| panic!("callback failure"));
  let inner_done = done.clone();
  api::nop().with_lio(&lio).when_done(move |_| inner_done.set(true));
  let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
    lio.try_run().unwrap();
  }));
  assert!(run.is_err());
  assert!(!done.get(), "the second callback is after the panic");

  // Nothing is left in the backend: the carried completion alone must not
  // leave a blocking run waiting.
  let start = std::time::Instant::now();
  lio.run().unwrap();
  assert!(done.get());
  assert!(start.elapsed() < Duration::from_secs(1), "without waiting");
}

#[test]
fn nested_run_routes_the_rest_of_the_outer_batch() {
  let lio = Lio::new_with_backend(InOrderBackend::default(), 4).unwrap();
  let second_ran = Rc::new(Cell::new(false));

  // Both complete in the same run. The first drives the loop itself until
  // the second has run, which only a nested run can make happen.
  let (nested, seen) = (lio.clone(), second_ran.clone());
  api::nop().with_lio(&lio).when_done(move |_| {
    for _ in 0..3 {
      if seen.get() {
        return;
      }
      nested.run().unwrap();
    }
    panic!("the nested runs never routed the second completion");
  });
  let set = second_ran.clone();
  api::nop().with_lio(&lio).when_done(move |_| set.set(true));

  let start = std::time::Instant::now();
  lio.try_run().unwrap();
  assert!(second_ran.get());
  assert!(start.elapsed() < Duration::from_secs(1), "without waiting");
}

#[test]
fn nested_run_through_the_global_lio_routes_the_rest() {
  let lio = Lio::new_with_backend(InOrderBackend::default(), 4).unwrap();
  let _guard = lio::install_global(lio.clone());
  let second_ran = Rc::new(Cell::new(false));

  // Submitted through the global; driven through the handle that was
  // installed, which is the same driver.
  let (nested, seen) = (lio.clone(), second_ran.clone());
  api::nop().when_done(move |_| {
    for _ in 0..3 {
      if seen.get() {
        return;
      }
      nested.run().unwrap();
    }
    panic!("the nested runs never routed the second completion");
  });
  let set = second_ran.clone();
  api::nop().when_done(move |_| set.set(true));

  lio.try_run().unwrap();
  assert!(second_ran.get());
}
