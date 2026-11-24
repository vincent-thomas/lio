//! Poll-shaped operation models. **Prototype.**
//!
//! A [`Model`] is a state machine that describes a piece of I/O work. The
//! driver asks it one question, [`poll`](Model::poll): given what has
//! finished so far, can you make progress?
//!
//! - A *leaf* ([`Read`], [`Write`], [`Sleep`]) submits one operation through
//!   the [`Cx`] and keeps the [`Ticket`] it gets back. The buffer moves into
//!   the driver with the operation and comes back with the result.
//! - A *composite* ([`Copy`], [`Join`], anything you write) holds other
//!   models and polls them. It never sees a completion, so it cannot hand one
//!   operation's result to another.
//!
//! No model here contains `unsafe`, and a model you write needs none.
//!
//! ```
//! use lio::Lio;
//! use lio::api::resource::Resource;
//! use lio::backend::testing::ScriptedBackend;
//! use lio::model::{self, Cx, Model, Read, Step};
//!
//! /// Reads twice from the same resource, one read after the other.
//! struct ReadTwice {
//!   from: Resource,
//!   first: Option<usize>,
//!   read: Read<Vec<u8>>,
//! }
//!
//! impl Model for ReadTwice {
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
//!           self.read = Read::new(&self.from, buf);
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
//!   read: Read::new(&from, Vec::with_capacity(8)),
//!   from,
//!   first: None,
//! };
//! model::spawn(&lio, model, |total| assert_eq!(total.unwrap(), 6));
//! while lio.try_run().unwrap() > 0 {}
//! ```
//!
//! # What is prototype about this
//!
//! Each [`Ticket`] is backed by one ordinary driver registration, which owns
//! the buffer and the operation's scratch memory until the backend reports
//! it. That is the "driver-owned slot". A finished design would have the
//! driver poll the root model itself and keep the slots in one arena per
//! root, instead of going through a callback and two `Rc`s per operation.

use std::cell::{Cell, RefCell};
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use crate::api::op::OneshotOpModel;
use crate::api::ops;
use crate::api::resource::{AsResource, Resource};
use crate::registration::Registration;
use crate::{BufResult, IoBufMutVec, IoBufVec, Lio};

/// A piece of I/O work the driver runs by polling it.
pub trait Model {
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

/// What one [`Model::poll`] call reports.
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
/// surrounding [`Model::poll`].
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

/// The driver's side of one [`Model::poll`] call.
///
/// The only way to start an operation, and the only place a result comes
/// from. Only the driver creates one.
pub struct Cx<'a> {
  lio: &'a Lio,
  root: &'a Rc<dyn Wake>,
}

impl Cx<'_> {
  /// Starts a read into `buf`, at `offset` or at the current position.
  pub fn read<B>(
    &mut self,
    from: &impl AsResource,
    buf: B,
    offset: Option<u64>,
  ) -> Ticket<BufResult<usize, B>>
  where
    B: IoBufMutVec + Send + Sync + 'static,
  {
    self.submit(ops::Read::new(from.as_resource().clone(), buf, offset))
  }

  /// Starts a write of `buf`, at `offset` or at the current position.
  pub fn write<B>(
    &mut self,
    to: &impl AsResource,
    buf: B,
    offset: Option<u64>,
  ) -> Ticket<BufResult<usize, B>>
  where
    B: IoBufVec + Send + Sync + 'static,
  {
    self.submit(ops::Write::new(to.as_resource().clone(), buf, offset))
  }

  /// Starts a timer.
  pub fn sleep(&mut self, duration: Duration) -> Ticket<io::Result<()>> {
    self.submit(ops::Sleep::new(duration))
  }

  /// The result of the operation `ticket` stands for, once, when it has
  /// finished.
  pub fn take<T>(&mut self, ticket: &mut Ticket<T>) -> Option<T> {
    ticket.slot.borrow_mut().take()
  }

  /// Moves `op`, with the buffers it owns, into a driver registration.
  ///
  /// Crate-private: the request types are lio's own, so the result a ticket
  /// yields is always the result of the operation that was submitted for it.
  fn submit<T>(&mut self, op: T) -> Ticket<T::Item>
  where
    T: OneshotOpModel,
  {
    let slot = Rc::new(RefCell::new(None));
    let filled = Rc::clone(&slot);
    let root = Rc::clone(self.root);
    let on_done = move |item: T::Item| {
      *filled.borrow_mut() = Some(item);
      Rc::clone(&root).wake();
    };

    let id = if self.lio.is_full() {
      // Over capacity the operation waits in the driver's queue. It has no
      // id until it leaves the queue, so it cannot be cancelled early.
      self.lio.schedule_or_queue(move |arena| {
        Registration::new_callback_in(arena, on_done, op)
      });
      None
    } else {
      let scheduled = self.lio.schedule_with(move |arena| {
        Registration::new_callback_in(arena, on_done, op)
      });
      match scheduled {
        Ok(id) => Some(id),
        Err(err) => {
          unreachable!("lio rejected an operation it had capacity for: {err}")
        }
      }
    };

    Ticket { slot, id, lio: self.lio.clone() }
  }
}

/// A receipt for one submitted operation.
///
/// Created by a [`Cx`], and neither copied nor built by hand. Its type is
/// the result that operation produces. Dropping it before the result is
/// taken cancels the operation; the driver frees the buffer once the backend
/// is done with it.
pub struct Ticket<T> {
  slot: Rc<RefCell<Option<T>>>,
  id: Option<u64>,
  lio: Lio,
}

impl<T> Drop for Ticket<T> {
  fn drop(&mut self) {
    let Some(id) = self.id else { return };
    // The registration's callback shares the slot. Once it is gone the
    // operation has finished and `id` may belong to a later one.
    if Rc::strong_count(&self.slot) == 1 || self.slot.borrow().is_some() {
      return;
    }
    self.lio.cancel_stream(id);
  }
}

// ============================================================================
// Running a root model
// ============================================================================

trait Wake {
  /// Polls the root model again: one of its operations finished.
  fn wake(self: Rc<Self>);

  /// Drops the root model, cancelling what it has in flight.
  fn cancel(&self);
}

struct Root<M: Model, F> {
  lio: Lio,
  model: RefCell<Option<M>>,
  on_item: RefCell<F>,
  /// A poll is running further up the stack.
  polling: Cell<bool>,
  /// An operation finished while a poll was running: poll once more.
  woken: Cell<bool>,
  cancelled: Cell<bool>,
}

impl<M, F> Wake for Root<M, F>
where
  M: Model + 'static,
  F: FnMut(M::Item) + 'static,
{
  fn wake(self: Rc<Self>) {
    if self.polling.replace(true) {
      self.woken.set(true);
      return;
    }
    let root: Rc<dyn Wake> = self.clone();
    loop {
      self.woken.set(false);
      if self.cancelled.get() {
        self.take_model();
        break;
      }
      let step = {
        let mut model = self.model.borrow_mut();
        let Some(model) = model.as_mut() else { break };
        model.poll(&mut Cx { lio: &self.lio, root: &root })
      };
      match step {
        Step::Pending if self.woken.get() => {}
        Step::Pending => break,
        Step::Yield(item) => (self.on_item.borrow_mut())(item),
        Step::Done(item) => {
          self.take_model();
          (self.on_item.borrow_mut())(item);
          break;
        }
      }
    }
    self.polling.set(false);
  }

  fn cancel(&self) {
    self.cancelled.set(true);
    if !self.polling.get() {
      self.take_model();
    }
  }
}

impl<M: Model, F> Root<M, F> {
  /// Drops the model with the cell released: dropping its tickets calls
  /// back into the driver.
  fn take_model(&self) {
    let model = self.model.borrow_mut().take();
    drop(model);
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
}

/// Runs `model` on `lio`, calling `on_item` with every item it produces.
///
/// The first poll happens here. The rest happen while `lio` runs.
pub fn spawn<M>(
  lio: &Lio,
  model: M,
  on_item: impl FnMut(M::Item) + 'static,
) -> Task
where
  M: Model + 'static,
{
  let root: Rc<dyn Wake> = Rc::new(Root {
    lio: lio.clone(),
    model: RefCell::new(Some(model)),
    on_item: RefCell::new(on_item),
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
  M: Model + Unpin + 'static,
{
  Run { state: RunState::Idle(lio.clone(), model) }
}

/// The future [`run`] returns.
pub struct Run<M: Model> {
  state: RunState<M>,
}

enum RunState<M: Model> {
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
  M: Model + Unpin + 'static,
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

impl<M: Model> Drop for Run<M> {
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
enum Leaf<Req, Out> {
  Idle(Req),
  InFlight(Ticket<Out>),
  Done,
}

impl<Req, Out> Leaf<Req, Out> {
  fn poll(
    &mut self,
    cx: &mut Cx<'_>,
    submit: impl FnOnce(&mut Cx<'_>, Req) -> Ticket<Out>,
  ) -> Step<Out> {
    let mut ticket = match std::mem::replace(self, Leaf::Done) {
      Leaf::Idle(request) => submit(cx, request),
      Leaf::InFlight(ticket) => ticket,
      Leaf::Done => panic!("model polled after it was done"),
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

/// Reads into an owned buffer.
///
/// Produces the number of bytes read and the buffer.
pub struct Read<B> {
  leaf: Leaf<(Resource, B, Option<u64>), BufResult<usize, B>>,
}

impl<B> Read<B> {
  /// Reads from the current position.
  pub fn new(from: &impl AsResource, buf: B) -> Self {
    Self { leaf: Leaf::Idle((from.as_resource().clone(), buf, None)) }
  }

  /// Reads at `offset`.
  pub fn at(from: &impl AsResource, buf: B, offset: u64) -> Self {
    Self { leaf: Leaf::Idle((from.as_resource().clone(), buf, Some(offset))) }
  }
}

impl<B> Model for Read<B>
where
  B: IoBufMutVec + Send + Sync + 'static,
{
  type Item = BufResult<usize, B>;

  fn poll(&mut self, cx: &mut Cx<'_>) -> Step<Self::Item> {
    self.leaf.poll(cx, |cx, (from, buf, offset)| cx.read(&from, buf, offset))
  }
}

/// Writes an owned buffer.
///
/// Produces the number of bytes written and the buffer.
pub struct Write<B> {
  leaf: Leaf<(Resource, B, Option<u64>), BufResult<usize, B>>,
}

impl<B> Write<B> {
  /// Writes at the current position.
  pub fn new(to: &impl AsResource, buf: B) -> Self {
    Self { leaf: Leaf::Idle((to.as_resource().clone(), buf, None)) }
  }

  /// Writes at `offset`.
  pub fn at(to: &impl AsResource, buf: B, offset: u64) -> Self {
    Self { leaf: Leaf::Idle((to.as_resource().clone(), buf, Some(offset))) }
  }
}

impl<B> Model for Write<B>
where
  B: IoBufVec + Send + Sync + 'static,
{
  type Item = BufResult<usize, B>;

  fn poll(&mut self, cx: &mut Cx<'_>) -> Step<Self::Item> {
    self.leaf.poll(cx, |cx, (to, buf, offset)| cx.write(&to, buf, offset))
  }
}

/// Completes after a duration.
pub struct Sleep {
  leaf: Leaf<Duration, io::Result<()>>,
}

impl Sleep {
  pub fn new(duration: Duration) -> Self {
    Self { leaf: Leaf::Idle(duration) }
  }
}

impl Model for Sleep {
  type Item = io::Result<()>;

  fn poll(&mut self, cx: &mut Cx<'_>) -> Step<Self::Item> {
    self.leaf.poll(cx, |cx, duration| cx.sleep(duration))
  }
}

// ============================================================================
// Composites
// ============================================================================

/// Copies from one resource to another until the source is at its end.
///
/// Produces the number of bytes copied. Written with the public [`Model`]
/// API only: it is what a user-written composite looks like.
pub struct Copy {
  from: Resource,
  to: Resource,
  copied: u64,
  state: CopyState,
}

enum CopyState {
  Reading(Read<Vec<u8>>),
  Writing(Write<Vec<u8>>),
}

impl Copy {
  /// Copies through a buffer of `buf_len` bytes.
  pub fn new(
    from: &impl AsResource,
    to: &impl AsResource,
    buf_len: usize,
  ) -> Self {
    let from = from.as_resource().clone();
    Self {
      // Uninitialized capacity: the read reports how much of it was written.
      state: CopyState::Reading(Read::new(&from, Vec::with_capacity(buf_len))),
      from,
      to: to.as_resource().clone(),
      copied: 0,
    }
  }
}

impl Model for Copy {
  type Item = io::Result<u64>;

  fn poll(&mut self, cx: &mut Cx<'_>) -> Step<Self::Item> {
    loop {
      match &mut self.state {
        CopyState::Reading(read) => {
          let (result, buf) = ready!(read.poll(cx));
          match result {
            Ok(0) => return Step::Done(Ok(self.copied)),
            Ok(_) => self.state = CopyState::Writing(Write::new(&self.to, buf)),
            Err(err) => return Step::Done(Err(err)),
          }
        }
        CopyState::Writing(write) => {
          let (result, mut buf) = ready!(write.poll(cx));
          let written = match result {
            Ok(0) => {
              return Step::Done(Err(io::ErrorKind::WriteZero.into()));
            }
            Ok(written) => written,
            Err(err) => return Step::Done(Err(err)),
          };
          self.copied += written as u64;
          buf.drain(..written);
          self.state = if buf.is_empty() {
            CopyState::Reading(Read::new(&self.from, buf))
          } else {
            CopyState::Writing(Write::new(&self.to, buf))
          };
        }
      }
    }
  }
}

/// Runs two models at the same time and produces both items.
///
/// Both are polled in the same call, so their operations are in flight
/// together.
pub struct Join<A: Model, B: Model> {
  a: Side<A>,
  b: Side<B>,
}

enum Side<M: Model> {
  Running(M),
  Finished(M::Item),
  Taken,
}

impl<M: Model> Side<M> {
  fn poll(&mut self, cx: &mut Cx<'_>) {
    if let Side::Running(model) = self
      && let Step::Yield(item) | Step::Done(item) = model.poll(cx)
    {
      *self = Side::Finished(item);
    }
  }
}

impl<A: Model, B: Model> Join<A, B> {
  pub fn new(a: A, b: B) -> Self {
    Self { a: Side::Running(a), b: Side::Running(b) }
  }
}

impl<A: Model, B: Model> Model for Join<A, B> {
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
