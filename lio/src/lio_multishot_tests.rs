//! Operations whose backend reports several results for one submission
//! ([`CompletionFlags::MORE`]), as a multishot accept or receive does.

use std::cell::RefCell;
use std::future::Future;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Wake, Waker};

use crate::Lio;
use crate::api::op::{
  Action, Completion, CompletionFlags, OpModel, OpResult, StreamOpModel,
};
use crate::api::{Io, IoStream};
use crate::backend::op::Op;
use crate::backend::testing::{Checked, ScriptedBackend};

const ECANCELED: isize = -125;

/// Yields every result until its `finish_at`-th completion, which it
/// finishes with.
struct Multishot {
  finish_at: usize,
  completes: Arc<AtomicUsize>,
}

impl Multishot {
  fn new(finish_at: usize) -> (Self, Arc<AtomicUsize>) {
    let completes = Arc::new(AtomicUsize::new(0));
    (Self { finish_at, completes: completes.clone() }, completes)
  }
}

impl OpModel for Multishot {
  type Item = isize;

  fn action(&mut self) -> Action {
    Action::Io(Op::Nop)
  }

  fn complete(&mut self, completion: Completion) -> OpResult<isize> {
    let n = self.completes.fetch_add(1, Ordering::SeqCst) + 1;
    if n >= self.finish_at {
      OpResult::Done(completion.result)
    } else {
      OpResult::Yield(completion.result)
    }
  }
}

impl StreamOpModel for Multishot {}

/// Counts its wakes.
#[derive(Default)]
struct Wakes(AtomicUsize);

impl Wake for Wakes {
  fn wake(self: Arc<Self>) {
    self.0.fetch_add(1, Ordering::SeqCst);
  }
}

impl Wakes {
  fn count(&self) -> usize {
    self.0.load(Ordering::SeqCst)
  }
}

fn lio(cap: usize) -> (ScriptedBackend, Lio) {
  let backend = ScriptedBackend::new();
  let lio = Lio::new_with_backend(Checked::new(backend.clone()), cap).unwrap();
  (backend, lio)
}

/// Submits `model` with a callback recording its items.
fn with_callback(lio: &Lio, model: Multishot) -> Rc<RefCell<Vec<isize>>> {
  let items = Rc::new(RefCell::new(Vec::new()));
  let out = items.clone();
  Io::from_op(model)
    .with_lio(lio)
    .when_done(move |item| out.borrow_mut().push(item));
  items
}

fn poll_next(
  stream: &mut IoStream<Multishot>,
  waker: &Waker,
) -> Poll<Option<isize>> {
  let mut next = std::pin::pin!(stream.next());
  next.as_mut().poll(&mut Context::from_waker(waker))
}

#[test]
fn more_completions_reach_the_model_without_a_resubmit() {
  let (backend, lio) = lio(1);
  let (model, completes) = Multishot::new(3);
  let items = with_callback(&lio, model);
  let id = backend.pushed()[0];

  backend.complete_more(id, 3);
  backend.complete_more(id, 4);
  assert_eq!(lio.try_run().unwrap(), 2);
  assert_eq!(*items.borrow(), [3, 4]);
  assert_eq!(backend.pushed(), [id], "nothing submitted again");
  assert!(lio.is_full(), "the operation keeps its slot");

  backend.complete(id, 5);
  lio.try_run().unwrap();
  assert_eq!(*items.borrow(), [3, 4, 5]);
  assert_eq!(completes.load(Ordering::SeqCst), 3);
  assert!(!lio.is_full());
  assert!(backend.cancelled().is_empty());
}

#[test]
fn a_yield_on_a_final_completion_submits_again() {
  let (backend, lio) = lio(1);
  let (model, _) = Multishot::new(2);
  let items = with_callback(&lio, model);
  let first = backend.pushed()[0];

  backend.complete(first, 1);
  lio.try_run().unwrap();
  assert_eq!(*items.borrow(), [1]);
  assert_eq!(backend.pushed(), [first, first], "re-armed under the same id");

  backend.complete(first, 2);
  lio.try_run().unwrap();
  assert_eq!(*items.borrow(), [1, 2]);
  assert!(!lio.is_full());
}

#[test]
fn a_model_done_on_a_more_completion_cancels_its_operation() {
  let (backend, lio) = lio(1);
  let (model, completes) = Multishot::new(1);
  let items = with_callback(&lio, model);
  let id = backend.pushed()[0];

  backend.complete_more(id, 7);
  lio.try_run().unwrap();
  assert_eq!(*items.borrow(), [7]);
  assert_eq!(backend.cancelled(), [id]);
  assert!(lio.is_full(), "the backend still uses the operation's memory");

  // Given to the model, so it can release what they carry, but delivered
  // to nobody and cancelled only once.
  backend.complete_more(id, 8);
  lio.try_run().unwrap();
  assert_eq!(*items.borrow(), [7]);
  assert_eq!(completes.load(Ordering::SeqCst), 2);
  assert_eq!(backend.cancelled(), [id]);
  assert!(lio.is_full());

  backend.complete(id, ECANCELED);
  lio.try_run().unwrap();
  assert_eq!(*items.borrow(), [7]);
  assert_eq!(completes.load(Ordering::SeqCst), 3);
  assert_eq!(backend.pushed(), [id]);
  assert!(!lio.is_full());
}

#[test]
fn a_stream_done_early_ends_once_the_backend_finishes() {
  let (backend, lio) = lio(1);
  let wakes = Arc::new(Wakes::default());
  let waker = Waker::from(wakes.clone());
  let (model, _) = Multishot::new(1);
  let mut stream = IoStream::from_op(model).with_lio(&lio);
  assert!(poll_next(&mut stream, &waker).is_pending());
  let id = backend.pushed()[0];

  backend.complete_more(id, 7);
  lio.try_run().unwrap();
  assert_eq!(wakes.count(), 1);
  assert_eq!(poll_next(&mut stream, &waker), Poll::Ready(Some(7)));
  // The model is done, but its operation still runs: the stream waits.
  assert!(poll_next(&mut stream, &waker).is_pending());

  backend.complete_more(id, 8);
  lio.try_run().unwrap();
  assert_eq!(wakes.count(), 1, "nothing for the stream");
  assert!(poll_next(&mut stream, &waker).is_pending());

  backend.complete(id, ECANCELED);
  lio.try_run().unwrap();
  assert_eq!(wakes.count(), 2, "woken when the stream ends");
  assert_eq!(poll_next(&mut stream, &waker), Poll::Ready(None));
  assert_eq!(backend.cancelled(), [id]);
  assert!(!lio.is_full());
}

#[test]
fn dropping_a_stream_done_early_cancels_nothing_more() {
  let (backend, lio) = lio(1);
  let waker = Waker::noop();
  let (model, _) = Multishot::new(1);
  let mut stream = IoStream::from_op(model).with_lio(&lio);
  assert!(poll_next(&mut stream, waker).is_pending());
  let id = backend.pushed()[0];

  backend.complete_more(id, 7);
  lio.try_run().unwrap();
  drop(stream);
  assert_eq!(backend.cancelled(), [id]);

  backend.complete(id, ECANCELED);
  lio.try_run().unwrap();
  assert!(!lio.is_full());
}

#[test]
fn a_dropped_stream_discards_more_completions() {
  let (backend, lio) = lio(1);
  let wakes = Arc::new(Wakes::default());
  let waker = Waker::from(wakes.clone());
  let (model, completes) = Multishot::new(usize::MAX);
  let mut stream = IoStream::from_op(model).with_lio(&lio);
  assert!(poll_next(&mut stream, &waker).is_pending());
  let id = backend.pushed()[0];

  backend.complete_more(id, 1);
  lio.try_run().unwrap();
  assert_eq!(poll_next(&mut stream, &waker), Poll::Ready(Some(1)));
  drop(stream);
  assert_eq!(backend.cancelled(), [id]);

  backend.complete_more(id, 2);
  lio.try_run().unwrap();
  assert_eq!(completes.load(Ordering::SeqCst), 2);
  assert!(lio.is_full(), "kept until the final completion");

  backend.complete(id, ECANCELED);
  lio.try_run().unwrap();
  assert_eq!(wakes.count(), 1, "the dropped stream is not woken");
  assert_eq!(backend.pushed(), [id]);
  assert!(!lio.is_full());
}

#[test]
fn a_step_without_an_item_on_a_more_completion_keeps_the_waker() {
  /// Produces an item only on its final completion.
  struct LastOnly;

  impl OpModel for LastOnly {
    type Item = isize;

    fn action(&mut self) -> Action {
      Action::Io(Op::Nop)
    }

    fn complete(&mut self, completion: Completion) -> OpResult<isize> {
      if completion.flags.contains(CompletionFlags::MORE) {
        OpResult::Again
      } else {
        OpResult::Done(completion.result)
      }
    }
  }

  impl StreamOpModel for LastOnly {}

  let (backend, lio) = lio(1);
  let wakes = Arc::new(Wakes::default());
  let waker = Waker::from(wakes.clone());
  let mut stream = IoStream::from_op(LastOnly).with_lio(&lio);
  let mut cx = Context::from_waker(&waker);
  assert!(std::pin::pin!(stream.next()).as_mut().poll(&mut cx).is_pending());
  let id = backend.pushed()[0];

  backend.complete_more(id, 1);
  lio.try_run().unwrap();
  assert_eq!(wakes.count(), 0);
  assert_eq!(backend.pushed(), [id]);

  backend.complete(id, 2);
  lio.try_run().unwrap();
  assert_eq!(wakes.count(), 1, "the waker survived the step");
  let next = std::pin::pin!(stream.next()).as_mut().poll(&mut cx);
  assert_eq!(next, Poll::Ready(Some(2)));
}
