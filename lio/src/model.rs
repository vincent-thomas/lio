//! Operation models: I/O work the driver runs by polling it.
//!
//! An [`OpModel`] is a state machine that describes a piece of I/O work. The
//! driver asks it one question, [`poll`](OpModel::poll): given what has
//! finished so far, can you make progress?
//!
//! - Every single operation in [`api`](crate::api) is a model (a *leaf*).
//!   Polling it submits the operation, with the buffer it owns, to the
//!   driver. The buffer comes back with the result.
//! - A *composite* ([`Join`], [`io::copy`](crate::io::copy), everything in
//!   `fs` and `net` that takes more than one step, anything you write) holds
//!   other models and polls them. It only ever sees their items, so it
//!   cannot hand one operation's result to another.
//!
//! A model you write needs no `unsafe`: the only way to start an operation
//! is to poll one of lio's leaves, and a leaf only produces the result of
//! the operation it started.
//!
//! [`api::run`](crate::api::run) runs a model of yours the way lio's own
//! are run, and [`api::run_stream`](crate::api::run_stream) runs one that
//! yields. [`spawn`] here takes the driver and a callback, and gives you a
//! handle to cancel by.
//!
//! ```
//! use lio::Lio;
//! use lio::api::{self, ops, resource::Resource};
//! use lio::backend::testing::ScriptedBackend;
//! use lio::model::{self, Cx, OpModel, Step};
//!
//! /// Reads twice from the same resource, one read after the other.
//! struct ReadTwice {
//!   from: Resource,
//!   first: Option<usize>,
//!   read: ops::Read<Vec<u8>>,
//! }
//!
//! impl OpModel for ReadTwice {
//!   type Item = std::io::Result<usize>;
//!
//!   fn poll(&mut self, cx: &mut Cx<'_>) -> Step<Self::Item> {
//!     loop {
//!       let (result, buf) = lio::ready!(self.read.poll(cx));
//!       let n = match result {
//!         Ok(n) => n,
//!         Err(err) => return Step::Done(Err(err)),
//!       };
//!       match self.first {
//!         Some(first) => return Step::Done(Ok(first + n)),
//!         None => {
//!           self.first = Some(n);
//!           self.read = api::read(&self.from, buf).into_inner();
//!         }
//!       }
//!     }
//!   }
//! }
//!
//! let backend = ScriptedBackend::auto(3);
//! let lio = Lio::new_with_backend(backend, 4).unwrap();
//! let from = Resource::stdin();
//! let model = ReadTwice {
//!   read: api::read(&from, Vec::with_capacity(8)).into_inner(),
//!   from,
//!   first: None,
//! };
//! model::spawn(&lio, model, |total| assert_eq!(total.unwrap(), 6));
//! while lio.try_run().unwrap() > 0 {}
//! ```
//!
//! # Where a running model lives
//!
//! Running a model moves it into one of the driver's slots, where it stays
//! until it is done. The first operation it has in flight uses that same
//! slot, so a single operation, or a model that does one thing at a time,
//! takes one slot and allocates nothing. An operation started while that
//! one is still busy takes a slot of its own.

use std::cell::RefCell;
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

use crate::Lio;
use bumpalo::Bump;

use crate::driver::{OpRef, PlaceOp, Spawned, WeakLio};
use crate::registration::{
  ActionFn, CompleteFn, LendFn, Registration, ReturnFn,
};

/// A piece of I/O work the driver runs by polling it.
pub trait OpModel {
  /// What the model produces.
  type Item;

  /// Makes as much progress as possible without waiting.
  ///
  /// The driver calls this once when the model starts, and again each time
  /// an operation the model (or a model inside it) submitted finishes. A
  /// model that holds other models polls them from here, passing `cx` on.
  ///
  /// # Panics
  ///
  /// A model may panic when polled after it returned [`Step::Done`].
  fn poll(&mut self, cx: &mut Cx<'_>) -> Step<Self::Item>;
}

/// What one [`OpModel::poll`] call reports.
#[derive(Debug, PartialEq, Eq)]
pub enum Step<T> {
  /// Nothing to report. The model is polled again when something it
  /// submitted finishes.
  Pending,
  /// One item, with more to come. The model is polled again right away.
  Yield(T),
  /// The last item. The model is dropped.
  Done(T),
}

/// Takes the item out of a [`Step`], or returns [`Step::Pending`] from the
/// surrounding [`OpModel::poll`].
///
/// For a child that produces one item. A [`Step::Yield`] is unwrapped the
/// same way as a [`Step::Done`].
#[macro_export]
macro_rules! ready {
  ($step:expr) => {
    match $step {
      $crate::model::Step::Pending => return $crate::model::Step::Pending,
      $crate::model::Step::Yield(item) | $crate::model::Step::Done(item) => {
        item
      }
    }
  };
}

/// The driver's side of one [`OpModel::poll`] call.
///
/// Operations start through it and results come back through it. Only the
/// driver creates one, and only lio's own operation types use it: a model
/// passes it on to the models it polls.
pub struct Cx<'a> {
  lio: &'a Lio,
  /// The slot of the model being polled.
  task: u64,
}

impl<'a> Cx<'a> {
  pub(crate) fn new(lio: &'a Lio, task: u64) -> Self {
    Self { lio, task }
  }

  /// Whether the model being polled was asked to stop.
  pub(crate) fn stopped(&self) -> bool {
    self.lio.is_stopped(self.task)
  }

  /// The result of the operation `ticket` stands for, once, when it has
  /// finished. Until then, the model being polled is the one polled again
  /// when it has.
  fn take<T>(&mut self, ticket: &mut Ticket<T>) -> Option<T> {
    // A ticket names a slot of the driver it came from.
    if ticket.taken || !ticket.lio.is(self.lio) {
      return None;
    }
    // SAFETY: only `submit` makes a ticket, from the `OpRef` of an
    // operation whose result is `T`, and it was made on this driver.
    let out = unsafe { self.lio.take_op::<T>(&ticket.op, self.task) }?;
    ticket.taken = true;
    Some(out)
  }

  /// Moves `state`, the memory one operation uses, into the driver, and
  /// submits the operation `action` builds from it.
  ///
  /// `complete` reads the result out of that same memory. The two are
  /// written together, at one call site per operation (see
  /// [`api::ops`](crate::api::ops)), so the result a ticket yields is always
  /// the result of the operation that was submitted for it.
  #[inline]
  pub(crate) fn submit<S: 'static, Out: 'static>(
    &mut self,
    state: S,
    action: ActionFn<S>,
    complete: CompleteFn<S, Out>,
  ) -> Ticket<Out> {
    self.ticket(move |reg: &mut Registration, arena: &mut Bump, waiter| {
      reg.start_op(arena, state, action, complete, waiter)
    })
  }

  /// [`submit`](Self::submit) for an operation that lends a buffer.
  ///
  /// `action` describes the buffer to the backend in the regions it is
  /// given, and `complete` gets those regions back to tell how much of each
  /// was used. They are part of the operation's memory but not of `state`:
  /// they are made in the driver, where they stay.
  #[inline]
  pub(crate) fn submit_lending<S: 'static, R: 'static, Out: 'static>(
    &mut self,
    state: S,
    action: LendFn<S, R>,
    complete: ReturnFn<S, R, Out>,
  ) -> Ticket<Out> {
    self.ticket(move |reg: &mut Registration, arena: &mut Bump, waiter| {
      reg.start_lending_op(arena, state, action, complete, waiter)
    })
  }

  #[inline]
  fn ticket<Out>(&mut self, place: impl PlaceOp) -> Ticket<Out> {
    Ticket {
      op: self.lio.submit_op(self.task, place),
      lio: self.lio.downgrade(),
      taken: false,
      _out: PhantomData,
    }
  }
}

/// A receipt for one submitted operation.
///
/// Its type is the result that operation produces. It names the driver slot
/// that holds the operation's memory and, later, its result. Dropping it
/// before the result is taken cancels the operation; the driver frees the
/// memory once the backend is done with it.
pub(crate) struct Ticket<T> {
  op: OpRef,
  lio: WeakLio,
  taken: bool,
  _out: PhantomData<fn() -> T>,
}

impl<T> Drop for Ticket<T> {
  fn drop(&mut self) {
    if self.taken {
      return;
    }
    // Without a driver there is nothing left to cancel.
    if let Some(lio) = self.lio.upgrade() {
      lio.drop_op(&self.op);
    }
  }
}

// ============================================================================
// Running a model
// ============================================================================

/// A running model.
///
/// Dropping the handle leaves the model running. [`cancel`](Self::cancel)
/// stops it.
pub struct Task {
  lio: WeakLio,
  spawned: Spawned,
}

impl Task {
  /// Drops the model. Operations it has in flight are cancelled, and no
  /// further item is delivered.
  pub fn cancel(self) {
    self.stop();
  }

  /// [`cancel`](Self::cancel) for a handle that cannot be consumed.
  pub(crate) fn stop(&self) {
    if let Some(lio) = self.lio.upgrade() {
      lio.cancel(&self.spawned);
    }
  }
}

/// Runs `model` on `lio`, calling `on_item` with every item it produces.
///
/// The model moves into the driver, which polls it for the first time here
/// and then whenever one of its operations finishes, while `lio` runs.
/// Over the driver's capacity it starts once a slot is free.
pub fn spawn<M>(
  lio: &Lio,
  model: M,
  mut on_item: impl FnMut(M::Item) + 'static,
) -> Task
where
  M: OpModel + 'static,
{
  spawn_while(lio, model, move |item| {
    on_item(item);
    true
  })
}

/// [`spawn`] that stops the model once `on_item` returns `false`.
pub(crate) fn spawn_while<M>(
  lio: &Lio,
  model: M,
  on_item: impl FnMut(M::Item) -> bool + 'static,
) -> Task
where
  M: OpModel + 'static,
{
  Task { lio: lio.downgrade(), spawned: lio.spawn(model, on_item) }
}

/// The future of a model: what an [`Io`](crate::api::Io) turns into. It
/// resolves to the model's first item.
///
/// The model starts on the first poll, or once the driver has a free slot.
/// Dropping the future cancels it.
pub struct Run<M: OpModel> {
  state: RunState<M>,
}

impl<M: OpModel> Run<M> {
  pub(crate) fn new(lio: Lio, model: M) -> Self {
    Self { state: RunState::Idle(lio, model) }
  }
}

enum RunState<M: OpModel> {
  Idle(Lio, M),
  Running { shared: Rc<RefCell<Shared<M::Item>>>, task: Task },
  Done,
}

struct Shared<T> {
  item: Option<T>,
  waker: Option<Waker>,
}

impl<M> Future for Run<M>
where
  M: OpModel + Unpin + 'static,
{
  type Output = M::Item;

  fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<M::Item> {
    let this = &mut *self;
    match std::mem::replace(&mut this.state, RunState::Done) {
      RunState::Idle(lio, model) => {
        if lio.is_full() {
          // Wait for a slot; the driver wakes us when one frees up.
          lio.wait_for_capacity(cx.waker());
          this.state = RunState::Idle(lio, model);
          return Poll::Pending;
        }
        let shared = Rc::new(RefCell::new(Shared {
          item: None,
          waker: Some(cx.waker().clone()),
        }));
        let sink = Rc::clone(&shared);
        let task = spawn_while(&lio, model, move |item| {
          let waker = {
            let mut shared = sink.borrow_mut();
            shared.item = Some(item);
            shared.waker.take()
          };
          // Wake with the cell released: the waker may poll or drop this
          // future inline, and both borrow it.
          if let Some(waker) = waker {
            waker.wake();
          }
          // One item is all a future takes.
          false
        });
        this.state = RunState::Running { shared, task };
        // The model may have finished inside `spawn`.
        Pin::new(this).poll(cx)
      }
      RunState::Running { shared, task } => {
        let mut waiting = shared.borrow_mut();
        if let Some(item) = waiting.item.take() {
          return Poll::Ready(item);
        }
        match &waiting.waker {
          Some(waker) if waker.will_wake(cx.waker()) => {}
          _ => waiting.waker = Some(cx.waker().clone()),
        }
        drop(waiting);
        this.state = RunState::Running { shared, task };
        Poll::Pending
      }
      RunState::Done => panic!("model future polled after completion"),
    }
  }
}

impl<M: OpModel> Drop for Run<M> {
  fn drop(&mut self) {
    if let RunState::Running { shared, task } = &self.state {
      // The operation may still complete; nobody is left to wake then.
      shared.borrow_mut().waker = None;
      task.stop();
    }
  }
}

// ============================================================================
// Leaves
// ============================================================================

/// One operation: not started, in flight, or finished.
///
/// Every operation type in [`api::ops`](crate::api::ops) is one of these
/// around what the operation was asked to do.
pub(crate) enum Leaf<Req, Out> {
  Idle(Req),
  InFlight(Ticket<Out>),
  Done,
}

impl<Req, Out> Leaf<Req, Out> {
  /// Starts the operation with `submit` on the first poll, then waits for
  /// its result.
  pub(crate) fn poll(
    &mut self,
    cx: &mut Cx<'_>,
    submit: impl FnOnce(&mut Cx<'_>, Req) -> Ticket<Out>,
  ) -> Step<Out> {
    let mut ticket = match std::mem::replace(self, Leaf::Done) {
      Leaf::Idle(request) => submit(cx, request),
      Leaf::InFlight(ticket) => ticket,
      Leaf::Done => panic!("operation polled after it was done"),
    };
    match cx.take(&mut ticket) {
      Some(out) => Step::Done(out),
      None => {
        *self = Leaf::InFlight(ticket);
        Step::Pending
      }
    }
  }
}

// ============================================================================
// Composites
// ============================================================================

/// Runs two models at the same time and produces both items.
///
/// Both are polled in the same call, so their operations are in flight
/// together.
pub struct Join<A: OpModel, B: OpModel> {
  a: Side<A>,
  b: Side<B>,
}

enum Side<M: OpModel> {
  Running(M),
  Finished(M::Item),
  Taken,
}

impl<M: OpModel> Side<M> {
  fn poll(&mut self, cx: &mut Cx<'_>) {
    if let Side::Running(model) = self
      && let Step::Yield(item) | Step::Done(item) = model.poll(cx)
    {
      *self = Side::Finished(item);
    }
  }
}

impl<A: OpModel, B: OpModel> Join<A, B> {
  pub fn new(a: A, b: B) -> Self {
    Self { a: Side::Running(a), b: Side::Running(b) }
  }
}

impl<A: OpModel, B: OpModel> OpModel for Join<A, B> {
  type Item = (A::Item, B::Item);

  fn poll(&mut self, cx: &mut Cx<'_>) -> Step<Self::Item> {
    self.a.poll(cx);
    self.b.poll(cx);
    if !matches!((&self.a, &self.b), (Side::Finished(_), Side::Finished(_))) {
      return Step::Pending;
    }
    let a = std::mem::replace(&mut self.a, Side::Taken);
    let b = std::mem::replace(&mut self.b, Side::Taken);
    let (Side::Finished(a), Side::Finished(b)) = (a, b) else {
      unreachable!("both sides were finished above");
    };
    Step::Done((a, b))
  }
}
