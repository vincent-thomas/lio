//! Dropping a future or stream whose operation is still in the backend must
//! not free the memory that operation uses: the registration (op model,
//! buffers, arenas) lives until the backend reports the operation done.

mod common;

use std::cell::RefCell;
use std::future::{Future, IntoFuture};
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::task::{Context, Wake, Waker};
use std::time::Duration;

use lio::api::op::{
  Action, Completion, OneshotOpModel, OpModel, OpResult, StreamOpModel,
};
use lio::api::{Io, IoStream};
use lio::backend::op::Op;
use lio::backend::{IoBackend, OpCompleted};
use lio::{Lio, api};

/// Holds every operation until the test completes it, and records
/// cancellation requests.
#[derive(Default)]
struct Held {
  pushed: Vec<u64>,
  cancelled: Vec<u64>,
  complete: Vec<OpCompleted>,
  /// Makes `drain` report operations still running.
  stuck: bool,
}

#[derive(Clone, Default)]
struct HoldingBackend(Rc<RefCell<Held>>);

impl IoBackend for HoldingBackend {
  fn init(&mut self, _cap: usize) -> std::io::Result<()> {
    Ok(())
  }

  fn push(&mut self, id: u64, _op: Op, _step_bump: &mut bumpalo::Bump) {
    self.0.borrow_mut().pushed.push(id);
  }

  fn flush(&mut self) -> std::io::Result<()> {
    Ok(())
  }

  fn cancel(&mut self, id: u64) {
    self.0.borrow_mut().cancelled.push(id);
  }

  fn drain(&mut self) -> bool {
    !self.0.borrow().stuck
  }

  fn wait(
    &mut self,
    _timeout: Option<Duration>,
    completed: &mut Vec<OpCompleted>,
  ) -> std::io::Result<()> {
    completed.clear();
    completed.append(&mut self.0.borrow_mut().complete);
    Ok(())
  }
}

/// An op model that reports what happens to it.
struct Tracked {
  dropped: Arc<AtomicBool>,
  completions: Arc<AtomicUsize>,
}

impl OpModel for Tracked {
  type Item = isize;

  fn action(&mut self) -> Action {
    Action::Io(Op::Nop)
  }

  fn complete(&mut self, completion: Completion) -> OpResult<isize> {
    self.completions.fetch_add(1, Ordering::SeqCst);
    OpResult::Done(completion.result)
  }
}

impl OneshotOpModel for Tracked {}

impl Drop for Tracked {
  fn drop(&mut self) {
    self.dropped.store(true, Ordering::SeqCst);
  }
}

#[test]
fn dropped_future_keeps_its_op_alive_until_completion() {
  let backend = HoldingBackend::default();
  let lio = Lio::new_with_backend(backend.clone(), 1).unwrap();
  let dropped = Arc::new(AtomicBool::new(false));
  let completions = Arc::new(AtomicUsize::new(0));

  let mut future = Box::pin(
    Io::from_op(Tracked {
      dropped: dropped.clone(),
      completions: completions.clone(),
    })
    .with_lio(&lio)
    .into_future(),
  );
  let mut cx = Context::from_waker(Waker::noop());
  assert!(future.as_mut().poll(&mut cx).is_pending());
  lio.try_run().unwrap();
  let id = backend.0.borrow().pushed[0];

  drop(future);
  assert_eq!(backend.0.borrow().cancelled, vec![id], "the backend is asked");
  assert!(
    !dropped.load(Ordering::SeqCst),
    "the op's memory must outlive its operation in the backend"
  );
  assert!(lio.is_full(), "its slot stays taken until then");

  backend.0.borrow_mut().complete.push(OpCompleted::new(id, -1));
  lio.try_run().unwrap();
  assert!(dropped.load(Ordering::SeqCst), "freed once the backend is done");
  assert_eq!(
    completions.load(Ordering::SeqCst),
    1,
    "the op model still sees the result, to release what it produced"
  );
  assert!(!lio.is_full());
}

/// Submits a `Tracked` op, drops its future, then drops the driver.
fn drop_driver_with_op_in_flight(backend: &HoldingBackend) -> Arc<AtomicBool> {
  let lio = Lio::new_with_backend(backend.clone(), 1).unwrap();
  let dropped = Arc::new(AtomicBool::new(false));
  let mut future = Box::pin(
    Io::from_op(Tracked {
      dropped: dropped.clone(),
      completions: Arc::new(AtomicUsize::new(0)),
    })
    .with_lio(&lio)
    .into_future(),
  );
  let mut cx = Context::from_waker(Waker::noop());
  assert!(future.as_mut().poll(&mut cx).is_pending());
  lio.try_run().unwrap();
  drop(future);
  drop(lio);
  dropped
}

#[test]
fn dropping_the_driver_frees_ops_the_backend_drained() {
  let backend = HoldingBackend::default();
  let dropped = drop_driver_with_op_in_flight(&backend);
  assert!(dropped.load(Ordering::SeqCst), "the store releases the op");
}

#[test]
fn dropping_the_driver_leaks_ops_the_backend_could_not_drain() {
  let backend = HoldingBackend::default();
  backend.0.borrow_mut().stuck = true;
  let dropped = drop_driver_with_op_in_flight(&backend);
  assert!(
    !dropped.load(Ordering::SeqCst),
    "memory an operation may still write into must not be freed"
  );
}

#[test]
fn receiver_outliving_its_driver_never_completes() {
  let lio = Lio::new(4).unwrap();
  let mut receiver = api::sleep(Duration::from_secs(60)).with_lio(&lio).send();
  drop(lio);

  // The operation can no longer complete; that is not a lio bug.
  assert!(receiver.try_recv().is_none());
  assert!(receiver.try_recv().is_none(), "and keeps not completing");
  assert!(receiver.recv_timeout(Duration::from_millis(1)).is_none());

  let recv = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
    let _ = receiver.recv();
  }));
  let message = recv.unwrap_err();
  let message = message.downcast_ref::<&str>().map_or_else(
    || message.downcast_ref::<String>().cloned().unwrap_or_default(),
    |m| m.to_string(),
  );
  assert!(message.contains("dropped before the operation completed"));
}

#[test]
fn dropped_sleep_is_removed_at_once() {
  let backend = HoldingBackend::default();
  let lio = Lio::new_with_backend(backend.clone(), 1).unwrap();

  let mut future =
    Box::pin(api::sleep(Duration::from_secs(60)).with_lio(&lio).into_future());
  let mut cx = Context::from_waker(Waker::noop());
  assert!(future.as_mut().poll(&mut cx).is_pending());
  drop(future);

  // Timers live in the driver, not the backend.
  assert!(backend.0.borrow().cancelled.is_empty());
  assert!(!lio.is_full());
}

/// Yields a value for every completion, forever.
struct Endless;

impl OpModel for Endless {
  type Item = ();

  fn action(&mut self) -> Action {
    Action::Io(Op::Nop)
  }

  fn complete(&mut self, _: Completion) -> OpResult<()> {
    OpResult::Yield(())
  }
}

impl StreamOpModel for Endless {}

/// Drops the stream it holds when woken.
struct DropOnWake(RefCell<Option<IoStream<Endless>>>);

// SAFETY: the test drives everything on one thread.
unsafe impl Send for DropOnWake {}
// SAFETY: as above.
unsafe impl Sync for DropOnWake {}

impl Wake for DropOnWake {
  fn wake(self: Arc<Self>) {
    drop(self.0.borrow_mut().take());
  }
}

#[test]
fn stream_dropped_by_its_wake_up_is_not_resubmitted() {
  let backend = HoldingBackend::default();
  let lio = Lio::new_with_backend(backend.clone(), 1).unwrap();

  let holder = Arc::new(DropOnWake(RefCell::new(None)));
  let waker = Waker::from(holder.clone());
  let mut cx = Context::from_waker(&waker);
  let mut stream = IoStream::from_op(Endless).with_lio(&lio);
  {
    let mut next = Box::pin(stream.next());
    assert!(next.as_mut().poll(&mut cx).is_pending());
  }
  *holder.0.borrow_mut() = Some(stream);
  lio.try_run().unwrap();
  let id = backend.0.borrow().pushed[0];

  // The completion wakes the task, which drops the stream mid-delivery.
  backend.0.borrow_mut().complete.push(OpCompleted::new(id, 0));
  lio.try_run().unwrap();
  assert!(holder.0.borrow().is_none(), "the wake-up dropped the stream");
  assert_eq!(backend.0.borrow().pushed, vec![id], "no next step is pushed");
  assert!(!lio.is_full(), "its slot is free");
}

#[test]
fn dropped_accept_frees_its_slot() {
  // A real backend: an accept with no client never completes on its own.
  let lio = Lio::new(1).unwrap();
  let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
  listener.set_nonblocking(true).unwrap();
  let listener = common::socket_resource(listener);

  let mut accept: Pin<Box<_>> =
    Box::pin(api::accept(&listener).with_lio(&lio).into_future());
  let mut cx = Context::from_waker(Waker::noop());
  assert!(accept.as_mut().poll(&mut cx).is_pending());
  lio.run_timeout(Duration::from_millis(10)).unwrap();
  assert!(accept.as_mut().poll(&mut cx).is_pending());
  drop(accept);

  for _ in 0..200 {
    if !lio.is_full() {
      return;
    }
    lio.run_timeout(Duration::from_millis(5)).unwrap();
  }
  panic!("the cancelled accept never released its slot");
}

#[test]
fn stream_on_the_global_lio_cancels_on_the_driver_it_ran_on() {
  let first = Lio::new(1).unwrap();
  let guard = lio::install_global(first.clone());
  // Submitted through the global, so on `first`.
  let mut stream = api::interval(Duration::from_secs(3600));
  let mut next = Box::pin(stream.next());
  let mut cx = Context::from_waker(Waker::noop());
  assert!(next.as_mut().poll(&mut cx).is_pending());
  drop(next);
  assert!(first.is_full());

  // Another driver becomes the global; its first op gets the same id.
  drop(guard);
  let second = Lio::new(1).unwrap();
  let _guard = lio::install_global(second.clone());
  let mut sleep = Box::pin(
    api::sleep(Duration::from_secs(60)).with_lio(&second).into_future(),
  );
  assert!(sleep.as_mut().poll(&mut cx).is_pending());

  drop(stream);
  assert!(!first.is_full(), "the stream's own operation was cancelled");
  assert!(second.is_full(), "the other driver's operation was left alone");
}
