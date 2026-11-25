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
//! [`Io`](crate::api::Io) runs lio's own models. [`spawn`] and [`run`] run
//! any model, including yours.
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
//! # Cost
//!
//! An operation run on its own through [`Io`](crate::api::Io) is registered
//! with the driver directly and allocates nothing. Inside a composite, each
//! operation is one driver registration plus one small allocation that
//! carries its result back to the model.

use std::any::TypeId;
use std::cell::{Cell, RefCell};
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

use bumpalo::Bump;

use crate::Lio;
use crate::driver::{QueueToken, WeakLio};
use crate::registration::{ActionFn, Callback, CompleteFn, Registration};

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
  mode: Mode<'a>,
}

enum Mode<'a> {
  /// Polling a root model: a result comes back through a [`Ticket`], and
  /// the root is polled again when it has.
  Root(&'a Rc<dyn Wake>),
  /// Running one operation on its own: its result goes straight to a
  /// callback, and there is no model to poll again.
  Direct(Direct<'a>),
}

/// The callback of an operation run on its own, behind the type of the
/// result it takes.
struct Direct<'a> {
  /// The `Out` of the `&mut dyn Deliver<Out>` that `deliver` points to.
  out: TypeId,
  deliver: *mut (),
  /// Whether the operation may wait in the driver's queue.
  queue: bool,
  _deliver: PhantomData<&'a mut ()>,
}

/// Hands a callback to the registration of the operation it belongs to.
trait Deliver<Out> {
  /// Moves the callback into the operation's arena.
  fn place(&mut self, arena: &mut Bump) -> Callback<Out>;

  /// Moves the callback to the heap, for an operation that has to wait for
  /// a slot.
  fn boxed(&mut self) -> Box<dyn Fn(Out)>;
}

impl<Out, F: Fn(Out) + 'static> Deliver<Out> for Option<F> {
  #[inline]
  fn place(&mut self, arena: &mut Bump) -> Callback<Out> {
    Callback::new_in(arena, self.taken())
  }

  fn boxed(&mut self) -> Box<dyn Fn(Out)> {
    Box::new(self.taken())
  }
}

trait Taken<F> {
  fn taken(&mut self) -> F;
}

impl<F> Taken<F> for Option<F> {
  fn taken(&mut self) -> F {
    let Some(f) = self.take() else {
      unreachable!("one operation run on its own submitted twice");
    };
    f
  }
}

impl<'a> Cx<'a> {
  /// The result of the operation `ticket` stands for, once, when it has
  /// finished.
  fn take<T>(&mut self, ticket: &mut Ticket<T>) -> Option<T> {
    match &mut ticket.kind {
      TicketKind::Rooted { slot, .. } => slot.borrow_mut().take(),
      TicketKind::Direct { .. } => None,
    }
  }

  /// Moves `state`, the memory one operation uses, into a driver
  /// registration, and submits the operation `action` builds from it.
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
    let lio = self.lio;
    let kind = match &mut self.mode {
      Mode::Root(root) => {
        let slot = Rc::new(RefCell::new(None));
        let filled = Rc::clone(&slot);
        let root = Rc::clone(root);
        let on_done = move |out: Out| {
          *filled.borrow_mut() = Some(out);
          Rc::clone(&root).notify();
        };
        let init = move |arena: &mut Bump| {
          let callback = Callback::new_in(arena, on_done);
          Registration::new_in(arena, state, action, complete, callback)
        };
        let id = if lio.is_full() {
          // Over capacity the operation waits in the driver's queue.
          TicketId::Queued(lio.queue_abandonable(init))
        } else {
          TicketId::Scheduled(scheduled(lio.schedule_with(init)))
        };
        TicketKind::Rooted { slot, id, lio: lio.downgrade() }
      }
      Mode::Direct(direct) => {
        let queue = direct.queue;
        let deliver = direct.deliver::<Out>();
        if queue && lio.is_full() {
          // Over capacity the operation waits in the driver's queue, and
          // its callback on the heap.
          let on_done = deliver.boxed();
          lio.schedule_or_queue(move |arena| {
            let callback = Callback::new_in(arena, on_done);
            Registration::new_in(arena, state, action, complete, callback)
          });
          TicketKind::Direct { id: None }
        } else {
          let id = scheduled(lio.schedule_with(|arena| {
            let callback = deliver.place(arena);
            Registration::new_in(arena, state, action, complete, callback)
          }));
          TicketKind::Direct { id: Some(id) }
        }
      }
    };
    Ticket { kind }
  }
}

impl Direct<'_> {
  /// The callback, as the one for an operation that produces `Out`.
  #[inline]
  fn deliver<Out: 'static>(&mut self) -> &mut dyn Deliver<Out> {
    assert!(
      self.out == TypeId::of::<Out>(),
      "an operation run on its own submitted another operation's result type"
    );
    // SAFETY: `deliver` was made in `run_direct` from a
    // `&mut &mut dyn Deliver<Out>` for the `Out` whose `TypeId` was just
    // checked, and that reference is borrowed for as long as this `Direct`
    // lives.
    unsafe { &mut **(self.deliver as *mut &mut dyn Deliver<Out>) }
  }
}

#[inline]
fn scheduled(scheduled: std::io::Result<u64>) -> u64 {
  match scheduled {
    Ok(id) => id,
    Err(err) => {
      unreachable!("lio rejected an operation it had capacity for: {err}")
    }
  }
}

/// Runs one operation on its own: `submit` starts it through the [`Cx`] it
/// is given, and `on_done` receives its result.
///
/// With `queue`, an operation over capacity waits in the driver's queue.
/// Without it the driver must have a free slot. Returns the operation's id
/// when it has a slot.
#[inline]
pub(crate) fn run_direct<Req, Out: 'static>(
  lio: &Lio,
  request: Req,
  on_done: impl Fn(Out) + 'static,
  queue: bool,
  submit: impl FnOnce(&mut Cx<'_>, Req) -> Ticket<Out>,
) -> Option<u64> {
  let mut on_done = Some(on_done);
  let mut deliver: &mut dyn Deliver<Out> = &mut on_done;
  let mut cx = Cx {
    lio,
    mode: Mode::Direct(Direct {
      out: TypeId::of::<Out>(),
      deliver: (&raw mut deliver).cast(),
      queue,
      _deliver: PhantomData,
    }),
  };
  let ticket = submit(&mut cx, request);
  match ticket.kind {
    TicketKind::Direct { id } => id,
    TicketKind::Rooted { .. } => {
      unreachable!("an operation run on its own was given a rooted ticket")
    }
  }
}

/// A receipt for one submitted operation.
///
/// Its type is the result that operation produces. Dropping it before the
/// result is taken cancels the operation; the driver frees the buffer once
/// the backend is done with it.
pub(crate) struct Ticket<T> {
  kind: TicketKind<T>,
}

enum TicketKind<T> {
  /// Submitted by a model: the result arrives in `slot`.
  Rooted { slot: Rc<RefCell<Option<T>>>, id: TicketId, lio: WeakLio },
  /// Run on its own: the result goes to a callback, and whoever holds that
  /// callback's other end cancels by id.
  Direct { id: Option<u64> },
}

enum TicketId {
  /// In a driver slot.
  Scheduled(u64),
  /// Waiting for a slot, or in one since.
  Queued(Rc<QueueToken>),
}

impl<T> Drop for Ticket<T> {
  fn drop(&mut self) {
    let TicketKind::Rooted { slot, id, lio } = &self.kind else { return };
    // The registration's callback shares the slot. Once it is gone the
    // operation has finished and its id may belong to a later one.
    if Rc::strong_count(slot) == 1 || slot.borrow().is_some() {
      return;
    }
    let id = match id {
      TicketId::Scheduled(id) => Some(*id),
      TicketId::Queued(token) => {
        // Still queued: it never reaches the backend.
        token.abandon();
        token.id()
      }
    };
    // Without a driver there is nothing left to cancel.
    if let Some(id) = id
      && let Some(lio) = lio.upgrade()
    {
      lio.cancel_stream(id);
    }
  }
}

// ============================================================================
// Running a root model
// ============================================================================

pub(crate) trait Wake {
  /// One of the root model's operations finished: have the driver poll it
  /// once the run that routed the completion has released its slot.
  fn notify(self: Rc<Self>);

  /// Polls the root model.
  fn wake(self: Rc<Self>);

  /// Drops the root model, cancelling what it has in flight.
  fn cancel(&self);
}

struct Root<M: OpModel, F> {
  /// Weak: the driver's registrations hold this root through their
  /// callbacks.
  lio: WeakLio,
  model: RefCell<Option<M>>,
  /// Receives the items. Dropped as soon as the model is: a stream's
  /// consumer learns from that drop that no further item is coming.
  on_item: RefCell<Option<F>>,
  /// Already waiting for the driver to poll it.
  notified: Cell<bool>,
  /// A poll is running further up the stack.
  polling: Cell<bool>,
  /// An operation finished while a poll was running: poll once more.
  woken: Cell<bool>,
  cancelled: Cell<bool>,
}

impl<M, F> Wake for Root<M, F>
where
  M: OpModel + 'static,
  F: FnMut(M::Item) -> bool + 'static,
{
  fn notify(self: Rc<Self>) {
    if !self.notified.replace(true)
      && let Some(lio) = self.lio.upgrade()
    {
      lio.wake_later(self);
    }
  }

  fn wake(self: Rc<Self>) {
    self.notified.set(false);
    if self.polling.replace(true) {
      self.woken.set(true);
      return;
    }
    let root: Rc<dyn Wake> = self.clone();
    loop {
      self.woken.set(false);
      // A model cannot outlive its driver: nothing would ever finish.
      let lio = self.lio.upgrade();
      let (false, Some(lio)) = (self.cancelled.get(), lio) else {
        self.finish();
        break;
      };
      let step = {
        let mut model = self.model.borrow_mut();
        let Some(model) = model.as_mut() else { break };
        model.poll(&mut Cx { lio: &lio, mode: Mode::Root(&root) })
      };
      match step {
        Step::Pending if self.woken.get() => {}
        Step::Pending => break,
        Step::Yield(item) => {
          let wanted = match self.on_item.borrow_mut().as_mut() {
            Some(on_item) => on_item(item),
            None => false,
          };
          if !wanted {
            self.cancelled.set(true);
          }
        }
        Step::Done(item) => {
          let model = self.model.borrow_mut().take();
          drop(model);
          let on_item = self.on_item.borrow_mut().take();
          if let Some(mut on_item) = on_item {
            on_item(item);
          }
          break;
        }
      }
    }
    self.polling.set(false);
  }

  fn cancel(&self) {
    self.cancelled.set(true);
    // A running poll sees the flag when it returns.
    if !self.polling.get() {
      self.finish();
    }
  }
}

impl<M: OpModel, F> Root<M, F> {
  /// Drops the model and the item sink, each with its cell released:
  /// dropping tickets calls back into the driver, and dropping the sink may
  /// wake a consumer that polls inline.
  fn finish(&self) {
    let model = self.model.borrow_mut().take();
    drop(model);
    let on_item = self.on_item.borrow_mut().take();
    drop(on_item);
  }
}

/// A running root model.
///
/// Dropping the handle leaves the model running. [`cancel`](Self::cancel)
/// stops it.
pub struct Task {
  root: Rc<dyn Wake>,
}

impl Task {
  /// Drops the model. Operations it has in flight are cancelled, and no
  /// further item is delivered.
  pub fn cancel(self) {
    self.root.cancel();
  }

  /// [`cancel`](Self::cancel) for a handle that cannot be consumed.
  pub(crate) fn stop(&self) {
    self.root.cancel();
  }
}

/// Runs `model` on `lio`, calling `on_item` with every item it produces.
///
/// The first poll happens here. The rest happen while `lio` runs.
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
  let root: Rc<dyn Wake> = Rc::new(Root {
    lio: lio.downgrade(),
    model: RefCell::new(Some(model)),
    on_item: RefCell::new(Some(on_item)),
    notified: Cell::new(false),
    polling: Cell::new(false),
    woken: Cell::new(false),
    cancelled: Cell::new(false),
  });
  Rc::clone(&root).wake();
  Task { root }
}

/// Runs `model` on `lio` as a future that resolves to its first item.
///
/// The model starts on the first poll. Dropping the future cancels it.
pub fn run<M>(lio: &Lio, model: M) -> Run<M>
where
  M: OpModel + Unpin + 'static,
{
  Run { state: RunState::Idle(lio.clone(), model) }
}

/// The future [`run`] returns.
pub struct Run<M: OpModel> {
  state: RunState<M>,
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
        let shared = Rc::new(RefCell::new(Shared {
          item: None,
          waker: Some(cx.waker().clone()),
        }));
        let sink = Rc::clone(&shared);
        let task = spawn(&lio, model, move |item| {
          let waker = {
            let mut shared = sink.borrow_mut();
            if shared.item.is_none() {
              shared.item = Some(item);
            }
            shared.waker.take()
          };
          if let Some(waker) = waker {
            waker.wake();
          }
        });
        this.state = RunState::Running { shared, task };
        // The model may have finished inside `spawn`.
        Pin::new(this).poll(cx)
      }
      RunState::Running { shared, task } => {
        let item = shared.borrow_mut().item.take();
        match item {
          Some(item) => {
            task.cancel();
            Poll::Ready(item)
          }
          None => {
            shared.borrow_mut().waker = Some(cx.waker().clone());
            this.state = RunState::Running { shared, task };
            Poll::Pending
          }
        }
      }
      RunState::Done => panic!("model future polled after completion"),
    }
  }
}

impl<M: OpModel> Drop for Run<M> {
  fn drop(&mut self) {
    if let RunState::Running { task, .. } =
      std::mem::replace(&mut self.state, RunState::Done)
    {
      task.cancel();
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

  /// What the operation was asked to do, when it has not started yet.
  pub(crate) fn into_idle(self) -> Option<Req> {
    match self {
      Leaf::Idle(request) => Some(request),
      Leaf::InFlight(_) | Leaf::Done => None,
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
