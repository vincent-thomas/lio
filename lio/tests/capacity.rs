//! Going over `Lio::new(cap)` must not panic: futures wait for a free slot,
//! and `when_done` / `send` / `send_with` queue the operation.

use std::cell::RefCell;
use std::future::{Future, IntoFuture};
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll, Wake, Waker};
use std::time::Duration;

use lio::api::op::{Action, Completion, OpModel, OpResult, StreamOpModel};
use lio::api::{Io, IoStream};
use lio::backend::op::Op;
use lio::backend::{IoBackend, OpCompleted};
use lio::{Lio, api};

/// Completes every queued no-op on the next wait, in submission order.
#[derive(Default)]
struct InOrderBackend {
  queued: Vec<u64>,
}

impl IoBackend for InOrderBackend {
  fn init(&mut self, _cap: usize) -> std::io::Result<()> {
    Ok(())
  }

  fn push(&mut self, id: u64, _op: Op, _step_bump: &mut bumpalo::Bump) {
    self.queued.push(id);
  }

  fn flush(&mut self) -> std::io::Result<()> {
    Ok(())
  }

  fn wait(
    &mut self,
    _timeout: Option<Duration>,
    completed: &mut Vec<OpCompleted>,
  ) -> std::io::Result<()> {
    completed.clear();
    completed.extend(self.queued.drain(..).map(|id| OpCompleted::new(id, 0)));
    Ok(())
  }
}

fn lio(cap: usize) -> Lio {
  Lio::new_with_backend(InOrderBackend::default(), cap).unwrap()
}

#[derive(Default)]
struct Flag(AtomicBool);

impl Wake for Flag {
  fn wake(self: Arc<Self>) {
    self.0.store(true, Ordering::SeqCst);
  }
}

#[test]
fn callbacks_beyond_capacity_queue_and_complete_in_order() {
  let lio = lio(2);
  let order = Rc::new(RefCell::new(Vec::new()));

  for i in 0..5 {
    let order = order.clone();
    api::nop().with_lio(&lio).when_done(move |_| order.borrow_mut().push(i));
  }
  assert!(lio.is_full(), "two in flight, three queued");

  for _ in 0..5 {
    lio.try_run().unwrap();
  }
  assert_eq!(*order.borrow(), vec![0, 1, 2, 3, 4]);
  assert!(!lio.is_full());
}

#[test]
fn channel_sends_beyond_capacity_all_arrive() {
  let lio = lio(1);
  let receivers: Vec<_> =
    (0..4).map(|_| api::nop().with_lio(&lio).send()).collect();

  for _ in 0..4 {
    lio.try_run().unwrap();
  }
  for mut receiver in receivers {
    receiver.try_recv().expect("queued operation completed").unwrap();
  }
}

#[test]
fn future_over_capacity_waits_for_a_slot() {
  let lio = lio(1);
  let woken = Arc::new(Flag::default());
  let waker = Waker::from(woken.clone());
  let mut cx = Context::from_waker(&waker);

  let mut first: Pin<Box<_>> =
    Box::pin(api::nop().with_lio(&lio).into_future());
  let mut second: Pin<Box<_>> =
    Box::pin(api::nop().with_lio(&lio).into_future());
  assert!(first.as_mut().poll(&mut cx).is_pending());
  // Every slot is taken: the second future waits instead of panicking.
  assert!(second.as_mut().poll(&mut cx).is_pending());
  assert!(lio.is_full());

  woken.0.store(false, Ordering::SeqCst);
  lio.try_run().unwrap();
  assert!(matches!(first.as_mut().poll(&mut cx), Poll::Ready(_)));
  assert!(woken.0.load(Ordering::SeqCst), "freeing a slot wakes the waiter");

  assert!(second.as_mut().poll(&mut cx).is_pending());
  lio.try_run().unwrap();
  assert!(matches!(second.as_mut().poll(&mut cx), Poll::Ready(_)));
}

#[test]
fn cancelling_a_future_frees_its_slot_for_a_waiter() {
  let lio = Lio::new(1).unwrap();
  let woken = Arc::new(Flag::default());
  let waker = Waker::from(woken.clone());
  let mut cx = Context::from_waker(&waker);

  let mut long: Pin<Box<_>> =
    Box::pin(api::sleep(Duration::from_secs(60)).with_lio(&lio).into_future());
  let mut waiting: Pin<Box<_>> =
    Box::pin(api::nop().with_lio(&lio).into_future());
  assert!(long.as_mut().poll(&mut cx).is_pending());
  assert!(waiting.as_mut().poll(&mut cx).is_pending());

  drop(long);
  assert!(woken.0.load(Ordering::SeqCst), "cancelling frees the slot");
  assert!(waiting.as_mut().poll(&mut cx).is_pending());
  for _ in 0..100 {
    lio.run_timeout(Duration::from_millis(5)).unwrap();
    if waiting.as_mut().poll(&mut cx).is_ready() {
      return;
    }
  }
  panic!("the waiting future never completed");
}

/// Yields one item, then finishes.
struct OneShotStream {
  done: bool,
}

impl OpModel for OneShotStream {
  type Item = u32;

  fn action(&mut self) -> Action {
    Action::Io(Op::Nop)
  }

  fn complete(&mut self, _: Completion) -> OpResult<u32> {
    if self.done {
      OpResult::Done(2)
    } else {
      self.done = true;
      OpResult::Yield(1)
    }
  }
}

impl StreamOpModel for OneShotStream {}

#[test]
fn stream_over_capacity_waits_instead_of_ending() {
  let lio = lio(1);
  let woken = Arc::new(Flag::default());
  let waker = Waker::from(woken.clone());
  let mut cx = Context::from_waker(&waker);

  // Occupy the only slot.
  let mut blocker: Pin<Box<_>> =
    Box::pin(Io::from_op(lio::api::ops::Nop).with_lio(&lio).into_future());
  assert!(blocker.as_mut().poll(&mut cx).is_pending());

  let mut stream =
    IoStream::from_op(OneShotStream { done: false }).with_lio(&lio);
  let mut next = Box::pin(stream.next());
  // Previously this reported `None`, ending the stream early.
  assert!(next.as_mut().poll(&mut cx).is_pending());

  let mut blocker = Some(blocker);
  for _ in 0..10 {
    lio.try_run().unwrap();
    if let Some(future) = blocker.as_mut()
      && future.as_mut().poll(&mut cx).is_ready()
    {
      blocker = None;
    }
    if let Poll::Ready(item) = next.as_mut().poll(&mut cx) {
      assert_eq!(item, Some(1));
      return;
    }
  }
  panic!("the stream never produced its item");
}
