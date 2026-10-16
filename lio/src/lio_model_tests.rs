//! Random scripts of submissions, completions, drops, polls and time steps
//! against the driver, checked against what each operation must deliver.
//!
//! The backend is a [`ScriptedBackend`] behind [`Checked`], so the contract
//! is checked from both sides on every call, and the driver's own
//! assertions run on every step.

use std::cell::RefCell;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Waker};
use std::time::Duration;

use proptest::prelude::*;

use crate::Lio;
use crate::api::Io;
use crate::api::op::{Action, Completion, OneshotOpModel, OpModel, OpResult};
use crate::api::resource::Resource;
use crate::backend::op::Op;
use crate::backend::testing::{Checked, ScriptedBackend};

/// An operation whose submission carries its tag, so the test can tell
/// which backend id belongs to which submission.
struct Tagged(u32);

impl OpModel for Tagged {
  type Item = (u32, isize);

  fn action(&mut self) -> Action {
    Action::Io(Op::Listen { fd: Resource::stdin(), backlog: self.0 as i32 })
  }

  fn complete(&mut self, completion: Completion) -> OpResult<(u32, isize)> {
    OpResult::Done((self.0, completion.result))
  }
}

impl OneshotOpModel for Tagged {}

#[derive(Clone, Debug)]
enum Step {
  Callback,
  Future,
  Sleep(u8),
  /// Completes the in-flight op at this index, successfully or not.
  Complete(usize, bool),
  /// Drops the pending future at this index.
  Drop(usize),
  Poll,
  Run,
  Advance(u16),
}

fn step() -> impl Strategy<Value = Step> {
  prop_oneof![
    3 => Just(Step::Callback),
    3 => Just(Step::Future),
    2 => (1..40u8).prop_map(Step::Sleep),
    5 => (any::<usize>(), any::<bool>()).prop_map(|(i, ok)| Step::Complete(i, ok)),
    2 => any::<usize>().prop_map(Step::Drop),
    3 => Just(Step::Poll),
    4 => Just(Step::Run),
    2 => (0..60u16).prop_map(Step::Advance),
  ]
}

const EIO: isize = -5;

#[derive(Debug, PartialEq)]
enum Delivered {
  Io(isize),
  Slept,
}

#[derive(Default)]
struct Expect {
  /// The result scripted for each tagged op, once completed.
  results: HashMap<u32, isize>,
  /// The earliest tick each sleep may wake at.
  wake_at: HashMap<u32, u64>,
  /// Tags whose future was dropped: they never deliver.
  dropped: Vec<u32>,
  submitted: Vec<u32>,
}

type Deliveries = Rc<RefCell<Vec<(u32, Delivered)>>>;
type Pending = Pin<Box<dyn Future<Output = ()>>>;

struct Harness {
  backend: ScriptedBackend,
  lio: Lio,
  futures: Vec<(u32, Pending)>,
  delivered: Deliveries,
  expect: Expect,
  next_tag: u32,
  now: u64,
}

impl Harness {
  fn new(cap: usize) -> Self {
    let backend = ScriptedBackend::new();
    let lio =
      Lio::new_with_backend(Checked::new(backend.clone()), cap).unwrap();
    crate::time::pause(&lio);
    Self {
      backend,
      lio,
      futures: Vec::new(),
      delivered: Rc::default(),
      expect: Expect::default(),
      next_tag: 0,
      now: 0,
    }
  }

  fn tag(&mut self) -> u32 {
    self.next_tag += 1;
    self.expect.submitted.push(self.next_tag);
    self.next_tag
  }

  fn push_future(
    &mut self,
    tag: u32,
    future: impl Future<Output = ()> + 'static,
  ) {
    self.futures.push((tag, Box::pin(future)));
    self.poll();
  }

  fn step(&mut self, step: Step) {
    match step {
      Step::Callback => {
        let tag = self.tag();
        let delivered = self.delivered.clone();
        Io::from_op(Tagged(tag)).with_lio(&self.lio).when_done(
          move |(tag, result)| {
            delivered.borrow_mut().push((tag, Delivered::Io(result)));
          },
        );
      }
      Step::Future => {
        let tag = self.tag();
        let io = Io::from_op(Tagged(tag)).with_lio(&self.lio);
        let delivered = self.delivered.clone();
        self.push_future(tag, async move {
          let (tag, result) = io.await;
          delivered.borrow_mut().push((tag, Delivered::Io(result)));
        });
      }
      Step::Sleep(ms) => {
        let tag = self.tag();
        self.expect.wake_at.insert(tag, self.now + u64::from(ms));
        let sleep = crate::api::sleep(Duration::from_millis(ms.into()))
          .with_lio(&self.lio);
        let delivered = self.delivered.clone();
        self.push_future(tag, async move {
          sleep.await.unwrap();
          delivered.borrow_mut().push((tag, Delivered::Slept));
        });
      }
      Step::Complete(i, ok) => {
        let in_flight = self.backend.in_flight();
        if in_flight.is_empty() {
          return;
        }
        let id = in_flight[i % in_flight.len()];
        let Some(Op::Listen { backlog, .. }) = self.backend.op(id) else {
          unreachable!("only tagged ops reach the backend");
        };
        let result = if ok { 0 } else { EIO };
        self.expect.results.insert(backlog as u32, result);
        self.backend.complete(id, result);
      }
      Step::Drop(i) => {
        if !self.futures.is_empty() {
          let (tag, future) = self.futures.remove(i % self.futures.len());
          self.expect.dropped.push(tag);
          drop(future);
        }
      }
      Step::Poll => self.poll(),
      Step::Run => {
        self.lio.try_run().unwrap();
      }
      Step::Advance(ticks) => {
        self.now += u64::from(ticks);
        self.lio.advance_time_by_ticks(ticks.into());
      }
    }
    self.check();
  }

  fn poll(&mut self) {
    let mut cx = Context::from_waker(Waker::noop());
    self
      .futures
      .retain_mut(|(_, future)| future.as_mut().poll(&mut cx).is_pending());
  }

  /// What was delivered so far is right, and nothing came twice.
  fn check(&self) {
    let delivered = self.delivered.borrow();
    let mut seen = Vec::new();
    for (tag, what) in delivered.iter() {
      assert!(!seen.contains(tag), "tag {tag} delivered twice");
      seen.push(*tag);
      match what {
        Delivered::Io(result) => assert_eq!(
          self.expect.results.get(tag),
          Some(result),
          "tag {tag} delivered a result the backend never reported"
        ),
        Delivered::Slept => assert!(
          self.now >= self.expect.wake_at[tag],
          "sleep {tag} woke before its duration passed"
        ),
      }
    }
  }

  /// Completes everything still running, then checks that every op that
  /// was not dropped delivered exactly once and that every slot is free.
  fn finish(mut self) {
    for _ in 0..64 {
      self.backend.complete_all(0);
      for tag in self.expect.submitted.clone() {
        self.expect.results.entry(tag).or_insert(0);
      }
      self.lio.try_run().unwrap();
      self.poll();
      self.check();
      if self.futures.is_empty() && self.backend.in_flight().is_empty() {
        break;
      }
      self.now += 100;
      self.lio.advance_time_by_ticks(100);
    }
    self.lio.try_run().unwrap();

    let delivered = self.delivered.borrow();
    for tag in &self.expect.submitted {
      let count = delivered.iter().filter(|(t, _)| t == tag).count();
      if self.expect.dropped.contains(tag) {
        // A future delivers by resolving, so it is never dropped after.
        assert_eq!(count, 0, "dropped tag {tag} delivered");
      } else {
        assert_eq!(count, 1, "tag {tag} delivered {count} times");
      }
    }
    assert!(self.futures.is_empty(), "every future resolved");
    assert!(self.backend.in_flight().is_empty());
    assert!(!self.lio.is_full(), "every slot is free again");
  }
}

proptest! {
  #[test]
  fn driver_delivers_every_operation_once(
    cap in 1..5usize,
    steps in prop::collection::vec(step(), 0..80),
  ) {
    let mut harness = Harness::new(cap);
    for step in steps {
      harness.step(step);
    }
    harness.finish();
  }
}
