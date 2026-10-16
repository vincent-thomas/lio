//! Every legal ordering of lifecycle events, for two operations, up to a
//! bounded number of steps.
//!
//! Random scripts (see `lio_model_tests`) find the common cases; this walks
//! all of the small ones: each pairing of consumer kinds, at a capacity
//! that makes the second op wait and one that does not, through every
//! interleaving of submit, complete (ok or error), drop, poll, run, time
//! steps and dropping the driver. Each sequence is replayed on a fresh
//! driver over `Checked<ScriptedBackend>`, so the contract checks, the
//! driver's assertions and the backend's buffer poisoning run throughout.
//!
//! Depth is `LIO_LIFECYCLE_DEPTH` (default 6).

use std::cell::RefCell;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::task::{Context, Waker};
use std::time::Duration;

use crate::Lio;
use crate::api::op::{
  Action, Completion, OneshotOpModel, OpModel, OpResult, StreamOpModel,
};
use crate::api::resource::Resource;
use crate::api::{Io, IoStream};
use crate::backend::op::Op;
use crate::backend::testing::{Checked, ScriptedBackend};

/// A stream ends after this many items, or at its first error.
const STREAM_ITEMS: u32 = 3;
const EIO: isize = -5;

/// An op whose submissions carry its tag, and which logs being dropped.
struct Tagged {
  tag: u32,
  stream: bool,
  completions: u32,
  drops: Arc<Mutex<Vec<u32>>>,
}

impl OpModel for Tagged {
  type Item = (u32, isize);

  fn action(&mut self) -> Action {
    Action::Io(Op::Listen { fd: Resource::stdin(), backlog: self.tag as i32 })
  }

  fn complete(&mut self, completion: Completion) -> OpResult<(u32, isize)> {
    self.completions += 1;
    let item = (self.tag, completion.result);
    if self.stream && completion.result >= 0 && self.completions < STREAM_ITEMS
    {
      OpResult::Yield(item)
    } else {
      OpResult::Done(item)
    }
  }
}

impl OneshotOpModel for Tagged {}
impl StreamOpModel for Tagged {}

impl Drop for Tagged {
  fn drop(&mut self) {
    self.drops.lock().unwrap().push(self.tag);
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
  Callback,
  Future,
  Stream,
  Sleep,
}

const KINDS: [Kind; 4] =
  [Kind::Callback, Kind::Future, Kind::Stream, Kind::Sleep];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Event {
  Submit(usize),
  Complete(usize, bool),
  Drop(usize),
  Poll,
  Run,
  Advance,
  DropDriver,
}

#[derive(Debug, PartialEq)]
enum Delivered {
  Io(isize),
  Slept,
}

type Pending = Pin<Box<dyn Future<Output = ()>>>;

#[derive(Default)]
struct OpState {
  submitted: bool,
  consumer: Option<Pending>,
  dropped: bool,
  /// Results the backend reported for this op, in order.
  results: Vec<isize>,
  delivered: Rc<RefCell<Vec<Delivered>>>,
  /// Backend ids of this op that were in flight when it was dropped.
  in_flight_at_drop: Vec<u64>,
  /// How many submissions it had made when it was dropped.
  pushed_at_drop: Option<usize>,
}

struct Harness {
  kinds: Vec<Kind>,
  backend: ScriptedBackend,
  lio: Option<Lio>,
  ops: Vec<OpState>,
  drops: Arc<Mutex<Vec<u32>>>,
  /// The op each registration id belongs to.
  owner: HashMap<u64, usize>,
}

impl Harness {
  fn new(kinds: &[Kind], cap: usize) -> Self {
    let backend = ScriptedBackend::new();
    let lio =
      Lio::new_with_backend(Checked::new(backend.clone()), cap).unwrap();
    crate::time::pause(&lio);
    Self {
      kinds: kinds.to_vec(),
      backend,
      lio: Some(lio),
      ops: kinds.iter().map(|_| OpState::default()).collect(),
      drops: Arc::default(),
      owner: HashMap::new(),
    }
  }

  /// Records the op behind every id the backend holds. Every submission is
  /// in flight right after the event that made it, since only `Complete`
  /// finishes one.
  fn observe(&mut self) {
    for id in self.backend.in_flight() {
      if let Some(Op::Listen { backlog, .. }) = self.backend.op(id) {
        self.owner.entry(id).or_insert(backlog as usize);
      }
    }
  }

  /// Submissions op `index` has made so far. Every step of an op is
  /// pushed under its registration's id, so count pushes, not ids.
  fn pushed(&self, index: usize) -> usize {
    let pushed = self.backend.pushed();
    pushed.iter().filter(|id| self.owner.get(id) == Some(&index)).count()
  }

  fn lio(&self) -> &Lio {
    self.lio.as_ref().expect("driver still alive")
  }

  /// Backend ids in flight for op `index`.
  fn in_flight(&self, index: usize) -> Vec<u64> {
    self
      .backend
      .in_flight()
      .into_iter()
      .filter(|&id| {
        matches!(self.backend.op(id), Some(Op::Listen { backlog, .. }) if backlog as usize == index)
      })
      .collect()
  }

  fn tagged(&self, index: usize, stream: bool) -> Tagged {
    Tagged {
      tag: index as u32,
      stream,
      completions: 0,
      drops: self.drops.clone(),
    }
  }

  /// The events that can happen now, skipping ones that would only repeat
  /// the previous event to no effect.
  fn events(&self, last: Option<Event>) -> Vec<Event> {
    if self.lio.is_none() {
      return Vec::new();
    }
    let mut events = Vec::new();
    for (index, op) in self.ops.iter().enumerate() {
      if !op.submitted {
        events.push(Event::Submit(index));
      }
      if !self.in_flight(index).is_empty() {
        events.push(Event::Complete(index, true));
        events.push(Event::Complete(index, false));
      }
      if op.consumer.is_some() {
        events.push(Event::Drop(index));
      }
    }
    let held = self.ops.iter().any(|op| op.consumer.is_some());
    let sleeping = self
      .ops
      .iter()
      .zip(&self.kinds)
      .any(|(op, kind)| op.submitted && *kind == Kind::Sleep);
    for (event, possible) in [
      (Event::Poll, held),
      (Event::Run, true),
      (Event::Advance, sleeping),
      (Event::DropDriver, true),
    ] {
      if possible && last != Some(event) {
        events.push(event);
      }
    }
    events
  }

  fn apply(&mut self, event: Event) {
    match event {
      Event::Submit(index) => self.submit(index),
      Event::Complete(index, ok) => {
        let id = self.in_flight(index)[0];
        let result = if ok { 0 } else { EIO };
        self.ops[index].results.push(result);
        self.backend.complete(id, result);
      }
      Event::Drop(index) => {
        let in_flight = self.in_flight(index);
        let pushed = self.pushed(index);
        let op = &mut self.ops[index];
        op.in_flight_at_drop = in_flight;
        op.pushed_at_drop = Some(pushed);
        op.dropped = true;
        drop(op.consumer.take());
      }
      Event::Poll => self.poll(),
      Event::Run => {
        self.lio().try_run().unwrap();
      }
      Event::Advance => self.lio().advance_time_by_ticks(1_000),
      Event::DropDriver => {
        for op in &mut self.ops {
          if op.consumer.is_some() {
            op.dropped = true;
          }
          drop(op.consumer.take());
        }
        self.lio = None;
      }
    }
    self.observe();
    self.check_deliveries();
  }

  fn submit(&mut self, index: usize) {
    let lio = self.lio().clone();
    let delivered = self.ops[index].delivered.clone();
    let consumer: Option<Pending> = match self.kinds[index] {
      Kind::Callback => {
        Io::from_op(self.tagged(index, false)).with_lio(&lio).when_done(
          move |(_, result)| delivered.borrow_mut().push(Delivered::Io(result)),
        );
        None
      }
      Kind::Future => {
        let io = Io::from_op(self.tagged(index, false)).with_lio(&lio);
        Some(Box::pin(async move {
          let (_, result) = io.await;
          delivered.borrow_mut().push(Delivered::Io(result));
        }))
      }
      Kind::Stream => {
        let mut stream =
          IoStream::from_op(self.tagged(index, true)).with_lio(&lio);
        Some(Box::pin(async move {
          while let Some((_, result)) = stream.next().await {
            delivered.borrow_mut().push(Delivered::Io(result));
          }
        }))
      }
      Kind::Sleep => {
        let sleep = crate::api::sleep(Duration::from_millis(5)).with_lio(&lio);
        Some(Box::pin(async move {
          sleep.await.unwrap();
          delivered.borrow_mut().push(Delivered::Slept);
        }))
      }
    };
    let op = &mut self.ops[index];
    op.submitted = true;
    op.consumer = consumer;
    self.poll();
  }

  fn poll(&mut self) {
    let mut cx = Context::from_waker(Waker::noop());
    for op in &mut self.ops {
      if let Some(consumer) = &mut op.consumer
        && consumer.as_mut().poll(&mut cx).is_ready()
      {
        op.consumer = None;
      }
    }
  }

  /// Every op delivered a prefix of what the backend reported for it.
  fn check_deliveries(&self) {
    for (index, (op, kind)) in self.ops.iter().zip(&self.kinds).enumerate() {
      let delivered = op.delivered.borrow();
      match kind {
        Kind::Sleep => assert!(delivered.len() <= 1, "op {index} slept twice"),
        Kind::Callback | Kind::Future => {
          assert!(delivered.len() <= 1, "op {index} delivered twice");
        }
        Kind::Stream => {}
      }
      for (n, item) in delivered.iter().enumerate() {
        if let Delivered::Io(result) = item {
          assert_eq!(
            op.results.get(n),
            Some(result),
            "op {index} item {n} is not what the backend reported"
          );
        }
      }
    }
  }

  /// Drives everything still running to the end, then checks what each
  /// op delivered, that every op model was dropped exactly once, that the
  /// backend was asked to cancel what was dropped mid-flight, and that
  /// every slot is free.
  fn finish(mut self) {
    // Once the driver is gone nothing more completes, by design: only what
    // was delivered before, and the drops, can be checked.
    let driver_dropped = self.lio.is_none();
    if !driver_dropped {
      for _ in 0..16 {
        for index in 0..self.ops.len() {
          for id in self.in_flight(index) {
            self.ops[index].results.push(0);
            self.backend.complete(id, 0);
          }
        }
        self.lio().try_run().unwrap();
        self.lio().advance_time_by_ticks(1_000);
        self.poll();
        self.observe();
        self.check_deliveries();
        let idle = self.backend.in_flight().is_empty()
          && self.ops.iter().all(|op| op.consumer.is_none());
        if idle {
          break;
        }
      }
      let lio = self.lio.take().unwrap();
      lio.try_run().unwrap();
      assert!(!lio.is_full(), "every slot is free again");
      for op in &self.ops {
        for id in &op.in_flight_at_drop {
          assert!(
            self.backend.cancelled().contains(id),
            "op {id} was dropped in flight, but never cancelled"
          );
        }
      }
    }
    assert!(self.backend.in_flight().is_empty(), "drained or completed");

    for (index, op) in self.ops.iter().enumerate() {
      if let Some(before) = op.pushed_at_drop {
        let after = self.pushed(index);
        assert_eq!(
          after, before,
          "op {index} was submitted again after its consumer was dropped"
        );
      }
    }

    let drops = self.drops.lock().unwrap();
    for (index, (op, kind)) in self.ops.iter().zip(&self.kinds).enumerate() {
      if op.submitted && *kind != Kind::Sleep {
        let times = drops.iter().filter(|&&tag| tag as usize == index).count();
        assert_eq!(times, 1, "op {index}'s model was dropped {times} times");
      }
      let delivered = op.delivered.borrow();
      if driver_dropped || op.dropped || !op.submitted {
        continue;
      }
      match kind {
        Kind::Callback | Kind::Future => {
          assert_eq!(delivered.len(), 1, "op {index} delivered once");
        }
        Kind::Stream => assert_eq!(
          delivered.len(),
          op.results.len(),
          "stream {index} delivered every item"
        ),
        Kind::Sleep => {
          assert_eq!(*delivered, [Delivered::Slept], "sleep {index} woke")
        }
      }
    }
  }
}

/// Replays `events` on a fresh driver, then either explores every next
/// event or, at the depth limit, finishes and checks the run.
fn explore(
  kinds: &[Kind],
  cap: usize,
  events: &mut Vec<Event>,
  depth: usize,
  runs: &mut usize,
) {
  let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
    let mut harness = Harness::new(kinds, cap);
    for &event in events.iter() {
      harness.apply(event);
    }
    let next = harness.events(events.last().copied());
    if events.len() == depth || next.is_empty() {
      harness.finish();
      None
    } else {
      Some(next)
    }
  }));
  match outcome {
    Err(panic) => {
      eprintln!("failing ordering: kinds={kinds:?} cap={cap} {events:?}");
      std::panic::resume_unwind(panic);
    }
    Ok(None) => *runs += 1,
    Ok(Some(next)) => {
      for event in next {
        events.push(event);
        explore(kinds, cap, events, depth, runs);
        events.pop();
      }
    }
  }
}

fn depth() -> usize {
  std::env::var("LIO_LIFECYCLE_DEPTH")
    .ok()
    .map_or(6, |depth| depth.parse().expect("LIO_LIFECYCLE_DEPTH is a number"))
}

#[test]
fn every_lifecycle_ordering_of_two_operations() {
  let depth = depth();
  let mut runs = 0;
  for first in KINDS {
    for second in KINDS {
      for cap in [1, 2] {
        explore(&[first, second], cap, &mut Vec::new(), depth, &mut runs);
      }
    }
  }
  eprintln!("checked {runs} orderings at depth {depth}");
}
