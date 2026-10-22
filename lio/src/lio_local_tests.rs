//! Futures and streams whose items wait in their registration, in the
//! driver's slot, until the consumer takes them.

use std::future::{Future, IntoFuture};
use std::pin::pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Waker};

use crate::Lio;
use crate::api::op::{Action, Completion, OpModel, OpResult, StreamOpModel};
use crate::api::{Io, IoStream};
use crate::backend::op::Op;
use crate::backend::testing::{Checked, ScriptedBackend};

/// Counts how many of its values were dropped.
struct Counted(u32, Arc<AtomicUsize>);

impl Drop for Counted {
  fn drop(&mut self) {
    self.1.fetch_add(1, Ordering::SeqCst);
  }
}

/// Yields an item per completion until its `items`-th, which it finishes
/// with.
struct Items {
  items: u32,
  seen: u32,
  drops: Arc<AtomicUsize>,
}

impl Items {
  fn new(items: u32) -> (Self, Arc<AtomicUsize>) {
    let drops = Arc::new(AtomicUsize::new(0));
    (Self { items, seen: 0, drops: drops.clone() }, drops)
  }
}

impl OpModel for Items {
  type Item = Counted;

  fn action(&mut self) -> Action {
    Action::Io(Op::Nop)
  }

  fn complete(&mut self, _: Completion) -> OpResult<Counted> {
    self.seen += 1;
    let item = Counted(self.seen, self.drops.clone());
    if self.seen >= self.items {
      OpResult::Done(item)
    } else {
      OpResult::Yield(item)
    }
  }
}

impl StreamOpModel for Items {}

fn lio(cap: usize) -> (ScriptedBackend, Lio) {
  let backend = ScriptedBackend::new();
  let lio = Lio::new_with_backend(Checked::new(backend.clone()), cap).unwrap();
  (backend, lio)
}

fn cx() -> Context<'static> {
  Context::from_waker(Waker::noop())
}

#[test]
fn a_result_waits_in_its_slot_until_the_future_takes_it() {
  let (backend, lio) = lio(1);
  let (model, drops) = Items::new(1);
  let mut future = pin!(Io::from_op(model).with_lio(&lio).into_future());
  assert!(future.as_mut().poll(&mut cx()).is_pending());

  backend.complete_all(0);
  lio.try_run().unwrap();
  assert!(lio.is_full(), "the result holds the slot");
  assert_eq!(drops.load(Ordering::SeqCst), 0);

  let Poll::Ready(item) = future.as_mut().poll(&mut cx()) else {
    panic!("the result is ready");
  };
  assert_eq!(item.0, 1);
  assert!(!lio.is_full(), "taking it freed the slot");
  assert!(backend.cancelled().is_empty());
}

#[test]
fn dropping_a_future_frees_its_slot_and_the_result_it_left() {
  let (backend, lio) = lio(1);
  let (model, drops) = Items::new(1);
  let mut future = Box::pin(Io::from_op(model).with_lio(&lio).into_future());
  assert!(future.as_mut().poll(&mut cx()).is_pending());
  backend.complete_all(0);
  lio.try_run().unwrap();

  drop(future);
  assert!(!lio.is_full());
  assert_eq!(drops.load(Ordering::SeqCst), 1, "the result was dropped");
  assert!(backend.cancelled().is_empty(), "nothing left in the backend");
}

#[test]
fn stream_items_queue_in_order_until_taken() {
  let (backend, lio) = lio(1);
  let (model, drops) = Items::new(3);
  let mut stream = IoStream::from_op(model).with_lio(&lio);
  assert!(pin!(stream.next()).poll(&mut cx()).is_pending());
  for _ in 0..3 {
    backend.complete_all(0);
    lio.try_run().unwrap();
  }
  assert!(lio.is_full(), "the items hold the slot");

  for expected in 1..=3 {
    let Poll::Ready(Some(item)) = pin!(stream.next()).poll(&mut cx()) else {
      panic!("item {expected} is ready");
    };
    assert_eq!(item.0, expected);
  }
  assert!(!lio.is_full(), "taking the last item freed the slot");
  assert!(matches!(pin!(stream.next()).poll(&mut cx()), Poll::Ready(None)));
  assert_eq!(drops.load(Ordering::SeqCst), 3);
}

#[test]
fn dropping_a_stream_drops_the_items_it_left() {
  let (backend, lio) = lio(1);
  let (model, drops) = Items::new(2);
  let mut stream = IoStream::from_op(model).with_lio(&lio);
  assert!(pin!(stream.next()).poll(&mut cx()).is_pending());
  for _ in 0..2 {
    backend.complete_all(0);
    lio.try_run().unwrap();
  }

  drop(stream);
  assert!(!lio.is_full());
  assert_eq!(drops.load(Ordering::SeqCst), 2);
}

#[test]
fn a_stream_ends_once_its_final_item_is_taken() {
  let (backend, lio) = lio(1);
  let (model, _) = Items::new(2);
  let mut stream = IoStream::from_op(model).with_lio(&lio);
  assert!(pin!(stream.next()).poll(&mut cx()).is_pending());

  backend.complete_all(0);
  lio.try_run().unwrap();
  let first = pin!(stream.next()).poll(&mut cx());
  assert!(matches!(first, Poll::Ready(Some(Counted(1, _)))));
  assert!(pin!(stream.next()).poll(&mut cx()).is_pending());

  backend.complete_all(0);
  lio.try_run().unwrap();
  let last = pin!(stream.next()).poll(&mut cx());
  assert!(matches!(last, Poll::Ready(Some(Counted(2, _)))));
  assert!(!lio.is_full());
  assert!(matches!(pin!(stream.next()).poll(&mut cx()), Poll::Ready(None)));
  assert_eq!(backend.pushed().len(), 2);
}

#[test]
fn a_stream_awaited_as_a_future_stops_after_its_first_item() {
  let (backend, lio) = lio(1);
  let (model, drops) = Items::new(u32::MAX);
  let mut future = pin!(Io::from_op(model).with_lio(&lio).into_future());
  assert!(future.as_mut().poll(&mut cx()).is_pending());
  let id = backend.pushed()[0];

  backend.complete_all(0);
  lio.try_run().unwrap();
  let Poll::Ready(item) = future.as_mut().poll(&mut cx()) else {
    panic!("the first item is ready");
  };
  assert_eq!(item.0, 1);
  // The model asked to go on; taking the future's one item cancels it.
  assert_eq!(backend.cancelled(), [id]);

  backend.complete_all(-125);
  lio.try_run().unwrap();
  assert!(!lio.is_full());
  assert_eq!(backend.pushed(), [id, id], "nothing after the cancel");
  drop(item);
  assert_eq!(drops.load(Ordering::SeqCst), 2, "the discarded item too");
}
