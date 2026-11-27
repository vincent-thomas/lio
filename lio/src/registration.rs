//! What one driver slot holds: a model being run, an operation, or both.
//!
//! A model that is run moves into a slot's arena and stays there until it is
//! done. The operations it starts need memory the backend can point into,
//! which must stay put until the backend reports them finished, whatever the
//! model does meanwhile. So that memory is the slot's too, not the model's:
//!
//! - the first operation a model has in flight uses the model's own slot,
//!   so a model that does one thing at a time takes one slot;
//! - an operation started while that one is busy takes a slot of its own.
//!
//! A slot is free again when its model is done and its operation's result
//! has been taken or dropped.

use std::alloc::Layout;
use std::mem::MaybeUninit;
use std::ptr::NonNull;
use std::task::Waker;

use bumpalo::Bump;

use crate::api::op::{Action, Completion};
use crate::buf::MAX_IOV_COUNT;
use crate::model::{Cx, Drive, Turn};

/// Builds the action an operation submits from the memory it owns.
///
/// Pointers in the action borrow that memory. The driver keeps it in place,
/// untouched, until the backend has finished the submission.
pub(crate) type ActionFn<S> = for<'a> fn(&'a mut S) -> Action<'a>;

/// Reads an operation's result out of its memory, given what the backend
/// reported for the action [`ActionFn`] built from that same memory.
pub(crate) type CompleteFn<S, Out> = fn(&mut S, Completion) -> Out;

/// Builds the action of an operation that lends a buffer: `regions` is
/// where it describes the buffer to the backend. It is part of the
/// operation's memory, and is not moved once the operation has a slot.
pub(crate) type LendFn<S, R> =
  for<'a> fn(&'a mut S, &'a mut Regions<R>) -> Action<'a>;

/// Reads the result of an operation that lent a buffer, given the regions
/// its [`LendFn`] lent.
pub(crate) type ReturnFn<S, R, Out> =
  fn(&mut S, &Regions<R>, Completion) -> Out;

/// The regions of a buffer that are lent to the backend, in the form `R`
/// the backend takes them in.
///
/// It has room for [`MAX_IOV_COUNT`], but only the ones in use are ever
/// written, and it is made where it stays: a single buffer, the common
/// case, does not pay for the rest.
pub(crate) struct Regions<R> {
  slots: [MaybeUninit<R>; MAX_IOV_COUNT],
  count: usize,
}

impl<R> Regions<R> {
  /// Makes `this` hold no regions.
  ///
  /// # Safety
  ///
  /// `this` must be valid for writes of a `Regions<R>`.
  unsafe fn init(this: *mut Self) {
    // SAFETY: the caller's promise. The slots are `MaybeUninit`, so
    // writing the count initializes the whole value.
    unsafe { (&raw mut (*this).count).write(0) };
  }

  /// No regions, for an operation that is attempted before it has a slot.
  #[inline]
  pub(crate) fn empty() -> Self {
    Self { slots: [const { MaybeUninit::uninit() }; MAX_IOV_COUNT], count: 0 }
  }

  /// Lends `regions`, in place of what was lent before. Regions past
  /// [`MAX_IOV_COUNT`] are left out.
  #[inline]
  pub(crate) fn lend(&mut self, regions: impl Iterator<Item = R>) {
    let mut count = 0;
    for (slot, region) in self.slots.iter_mut().zip(regions) {
      slot.write(region);
      count += 1;
    }
    self.count = count;
  }

  /// What is lent.
  #[inline]
  pub(crate) fn lent(&self) -> &[R] {
    // SAFETY: `lend` wrote the first `count` slots, and `MaybeUninit<R>`
    // has the layout of `R`.
    unsafe {
      std::slice::from_raw_parts(self.slots.as_ptr().cast(), self.count)
    }
  }

  /// What is lent, or one region from `empty` when nothing is: a message
  /// needs at least one.
  #[inline]
  pub(crate) fn lent_or(&mut self, empty: impl FnOnce() -> R) -> &[R] {
    if self.count == 0 {
      let [first, ..] = &mut self.slots;
      return std::slice::from_ref(first.write(empty()));
    }
    self.lent()
  }

  /// Where the first region is, for an operation that takes them as a
  /// pointer and a count.
  #[inline]
  pub(crate) fn as_ptr(&mut self) -> NonNull<R> {
    NonNull::from(&mut self.slots).cast()
  }

  /// How many regions are lent.
  #[inline]
  pub(crate) fn count(&self) -> usize {
    self.count
  }
}

/// Calls the closure with an operation's action, for any lifetime of that
/// action.
type WithActionFn = fn(*mut (), &mut dyn for<'a> FnMut(Action<'a>));

// ============================================================================
// The model a slot runs
// ============================================================================

/// A model and where its items go, in the slot's arena.
struct TaskPayload<M, F> {
  model: Option<M>,
  sink: Option<F>,
}

/// Where the model a future runs leaves its item, and what wakes that
/// future, in the slot's arena.
///
/// The future reaches it through the slot, knowing `T` but not the model:
/// it comes first in [`AwaitedPayload`].
#[repr(C)]
struct Mailbox<T> {
  /// First, so that the driver can set it without knowing `T`.
  waker: Option<Waker>,
  item: Option<T>,
}

/// A model run for a future, and that future's mailbox.
#[repr(C)]
struct AwaitedPayload<M: Drive> {
  mailbox: Mailbox<M::Item>,
  model: Option<M>,
}

/// What a future finds in its slot.
pub(crate) enum Mail<T> {
  /// The model's item.
  Item(T),
  /// Nothing yet. The waker that was replaced, if one was, to be dropped
  /// with the driver not borrowed.
  Waiting(Option<Waker>),
}

struct Task {
  payload: *mut (),
  poll_fn: fn(*mut (), &mut Cx<'_>) -> bool,
  drop_fn: fn(*mut ()),
  /// The model is being polled, or dropped, further up the stack.
  busy: bool,
  /// An operation finished while the model was busy: poll once more.
  woken: bool,
  /// Already in the driver's queue of models to poll.
  notified: bool,
  /// Its consumer is gone: drop the model at the next chance.
  cancelled: bool,
  /// The model and its sink have been dropped.
  finished: bool,
  /// A future still reads this slot's mailbox: the slot is not given back
  /// before it has taken its item, or is gone.
  held: bool,
}

/// A copy of what is needed to poll or drop a slot's model without holding
/// the slot: both run user code, which may use the driver.
#[derive(Clone, Copy)]
pub(crate) struct RawTask {
  payload: *mut (),
  poll_fn: fn(*mut (), &mut Cx<'_>) -> bool,
  drop_fn: fn(*mut ()),
}

impl RawTask {
  /// Polls the model until it has nothing more to do right now. Returns
  /// whether it is finished, and dropped with its sink.
  ///
  /// # Safety
  ///
  /// The registration this was copied from must be alive and marked busy by
  /// [`Registration::begin_poll`], so that nothing else touches its model.
  pub(crate) unsafe fn poll(self, cx: &mut Cx<'_>) -> bool {
    (self.poll_fn)(self.payload, cx)
  }

  /// Drops the model and its sink.
  ///
  /// # Safety
  ///
  /// The registration this was copied from must be alive and marked busy,
  /// so that nothing else touches its model.
  pub(crate) unsafe fn drop_model(self) {
    (self.drop_fn)(self.payload);
  }
}

// ============================================================================
// The operation a slot holds
// ============================================================================

/// The memory of one operation and, once it is finished, its result.
///
/// `result` comes first so that a ticket, which knows `Out` but not `S`, can
/// reach it through a pointer to this.
#[repr(C)]
struct OpPayload<S, Out> {
  result: Option<Out>,
  state: Option<S>,
  action: ActionFn<S>,
  complete: CompleteFn<S, Out>,
}

/// The memory of an operation that lends a buffer: as [`OpPayload`], with
/// the regions it lends.
#[repr(C)]
struct LendingPayload<S, R, Out> {
  result: Option<Out>,
  state: Option<S>,
  regions: Regions<R>,
  action: LendFn<S, R>,
  complete: ReturnFn<S, R, Out>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Stage {
  /// No operation: one may start.
  Free,
  /// Submitted, and not reported finished yet.
  InFlight,
  /// Finished: the result waits to be taken.
  Ready,
  /// Being read or dropped further up the stack.
  Busy,
}

struct OpCell {
  stage: Stage,
  /// Nobody will take the result: drop it when it arrives.
  abandoned: bool,
  /// Counts the operations this slot has held, so a receipt for an earlier
  /// one does not match a later one.
  seq: u64,
  /// The slot of the model to poll when the result is in.
  waiter: u64,
  payload: *mut (),
  with_action_fn: WithActionFn,
  complete_fn: fn(*mut (), Completion, bool),
  drop_fn: fn(*mut ()),
}

/// A copy of what is needed to finish a slot's operation without holding
/// the slot: reading a result may run user code.
#[derive(Clone, Copy)]
pub(crate) struct RawOp {
  payload: *mut (),
  complete_fn: fn(*mut (), Completion, bool),
  drop_fn: fn(*mut ()),
  keep: bool,
}

impl RawOp {
  /// Reads the operation's result out of its memory and drops that memory.
  /// The result is kept for its ticket, unless the operation was abandoned.
  ///
  /// # Safety
  ///
  /// The registration this was copied from must be alive, with its
  /// operation marked busy by [`Registration::begin_completion`].
  pub(crate) unsafe fn complete(self, completion: Completion) {
    (self.complete_fn)(self.payload, completion, self.keep);
  }

  /// Drops the operation's memory, or its untaken result.
  ///
  /// # Safety
  ///
  /// The registration this was copied from must be alive, with its
  /// operation marked busy by [`Registration::begin_drop`].
  pub(crate) unsafe fn drop_payload(self) {
    (self.drop_fn)(self.payload);
  }
}

/// What dropping a ticket asks of the driver.
pub(crate) enum Abandon {
  /// The ticket is for an operation this slot no longer holds.
  Stale,
  /// The operation is in flight: finish it early, and drop its result.
  InFlight,
  /// Its result is waiting: drop it.
  Ready,
}

/// What the driver does after polling a slot's model.
pub(crate) enum AfterPoll {
  /// Waiting on its operations.
  Pending,
  /// Woken while it was polled: poll again.
  Again,
  /// Its consumer went away while it was polled: drop the model.
  Cancelled,
  /// Done, and dropped.
  Finished,
}

// ============================================================================
// Registration
// ============================================================================

/// One driver slot.
// NOTE: Registration should **NEVER** impl Sync.
pub struct Registration {
  task: Option<Task>,
  cell: OpCell,
}

fn no_action(_: *mut (), _: &mut dyn for<'a> FnMut(Action<'a>)) {}
fn no_completion(_: *mut (), _: Completion, _: bool) {}
fn no_drop(_: *mut ()) {}

impl Registration {
  /// A slot with no model: it holds an operation started by a model whose
  /// own slot was busy.
  pub(crate) fn vacant() -> Self {
    Self {
      task: None,
      cell: OpCell {
        stage: Stage::Free,
        abandoned: false,
        seq: 0,
        waiter: 0,
        payload: std::ptr::null_mut(),
        with_action_fn: no_action,
        complete_fn: no_completion,
        drop_fn: no_drop,
      },
    }
  }

  /// A slot that runs `model`, passing every item it produces to `sink`
  /// until the model is done or `sink` returns `false`.
  pub(crate) fn running<M, F>(arena: &mut Bump, model: M, sink: F) -> Self
  where
    M: Drive + 'static,
    F: FnMut(M::Item) -> bool + 'static,
  {
    let payload: &mut TaskPayload<M, F> =
      arena.alloc(TaskPayload { model: Some(model), sink: Some(sink) });
    let mut registration = Self::vacant();
    registration.task = Some(Task {
      payload: (payload as *mut TaskPayload<M, F>).cast(),
      poll_fn: Self::poll_impl::<M, F>,
      drop_fn: Self::drop_task_impl::<M, F>,
      busy: false,
      woken: false,
      notified: false,
      cancelled: false,
      finished: false,
      held: false,
    });
    registration
  }

  /// A slot that runs `model` for a future: the model's item waits in the
  /// slot for that future. Nothing wakes it until
  /// [`arm_mail`](Self::arm_mail) says what does.
  ///
  /// The slot is held until [`take_mail`](Self::take_mail) has returned the
  /// item or [`release_mail`](Self::release_mail) was called.
  pub(crate) fn awaited<M>(arena: &mut Bump, model: M) -> Self
  where
    M: Drive + 'static,
  {
    let payload: &mut AwaitedPayload<M> = arena.alloc(AwaitedPayload {
      mailbox: Mailbox { item: None, waker: None },
      model: Some(model),
    });
    let mut registration = Self::vacant();
    registration.task = Some(Task {
      payload: (payload as *mut AwaitedPayload<M>).cast(),
      poll_fn: Self::poll_awaited_impl::<M>,
      drop_fn: Self::drop_awaited_impl::<M>,
      busy: false,
      woken: false,
      notified: false,
      cancelled: false,
      finished: false,
      held: true,
    });
    registration
  }

  fn poll_awaited_impl<M: Drive>(payload: *mut (), cx: &mut Cx<'_>) -> bool {
    let payload = payload.cast::<AwaitedPayload<M>>();
    // SAFETY: `payload` was created from an `AwaitedPayload<M>` in `awaited`
    // and lives in the slot's arena. The task is marked busy, so nothing
    // else touches the model.
    let turn = match unsafe { &mut (*payload).model } {
      Some(model) => model.drive(cx),
      None => return true,
    };
    let item = match turn {
      Turn::Pending => return false,
      // One item is all a future takes.
      Turn::Yield(item) | Turn::Done(item) => Some(item),
      Turn::End => None,
    };
    // SAFETY: as above. No reference into the payload is alive here.
    unsafe { (*payload).model = None };
    let Some(item) = item else { return true };
    // SAFETY: as above. The future reaches the mailbox only from the
    // driver's thread, through the slot, and not during this block.
    let waker = unsafe {
      let mailbox = &mut (*payload).mailbox;
      mailbox.item = Some(item);
      mailbox.waker.take()
    };
    // Woken with no reference to the mailbox alive: the waker may poll or
    // drop the future inline, and both reach it.
    if let Some(waker) = waker {
      waker.wake();
    }
    true
  }

  fn drop_awaited_impl<M: Drive>(payload: *mut ()) {
    let payload = payload.cast::<AwaitedPayload<M>>();
    // SAFETY: as in `poll_awaited_impl`. Each is taken, so nothing is
    // dropped twice.
    unsafe {
      (*payload).model = None;
      (*payload).mailbox.item = None;
      (*payload).mailbox.waker = None;
    }
  }

  /// Takes over the model of `made`, a registration that holds nothing
  /// else, for a slot that held an operation and ran no model.
  pub(crate) fn adopt(&mut self, mut made: Registration) {
    assert!(
      self.task.is_none() && made.cell.stage == Stage::Free,
      "driver: a slot that runs a model was given another"
    );
    self.task = made.task.take();
  }

  /// Whether this slot's model, if it runs one, is done and dropped.
  pub(crate) fn is_finished(&self) -> bool {
    self.task.as_ref().is_none_or(|task| task.finished)
  }

  /// Names what wakes the future this slot's model runs for, when its item
  /// is there.
  pub(crate) fn arm_mail(&mut self, waker: Waker) {
    let Some(task) = self.task.as_mut().filter(|task| task.held) else {
      unreachable!("a slot that runs no model for a future was armed");
    };
    // SAFETY: a held task's payload is an `AwaitedPayload`, which is
    // `repr(C)` and starts with its mailbox's waker, whatever its item is.
    // Nothing else refers to the mailbox: see `take_mail`.
    let before = unsafe { &mut *task.payload.cast::<Option<Waker>>() };
    assert!(before.is_none(), "a future's slot was armed twice");
    *before = Some(waker);
  }

  /// The item of the model this slot runs for a future, once it is there.
  /// Until then, `waker` is what wakes that future.
  ///
  /// # Safety
  ///
  /// This slot was made by [`awaited`](Self::awaited) for a model whose
  /// item is `T`.
  pub(crate) unsafe fn take_mail<T>(&mut self, waker: &Waker) -> Mail<T> {
    let Some(task) = self.task.as_mut() else {
      unreachable!("a future's slot runs no model");
    };
    // SAFETY: the payload is an `AwaitedPayload` whose item is `T`, which
    // is `repr(C)` with a `Mailbox<T>` first, and nothing else refers to
    // the mailbox: whoever polls the model holds no reference to it while
    // other code runs.
    let mailbox = unsafe { &mut *task.payload.cast::<Mailbox<T>>() };
    if let Some(item) = mailbox.item.take() {
      task.held = false;
      return Mail::Item(item);
    }
    // A finished model with no item left has nothing more to say: its
    // future waits for ever, and needs no waker.
    if task.finished && !task.busy {
      return Mail::Waiting(None);
    }
    match &mailbox.waker {
      Some(current) if current.will_wake(waker) => Mail::Waiting(None),
      _ => Mail::Waiting(mailbox.waker.replace(waker.clone())),
    }
  }

  /// The future of this slot is gone: lets go of the slot, and returns
  /// what was left in its mailbox, to be dropped with the driver not
  /// borrowed.
  ///
  /// # Safety
  ///
  /// As for [`take_mail`](Self::take_mail).
  pub(crate) unsafe fn release_mail<T>(
    &mut self,
  ) -> (Option<T>, Option<Waker>) {
    let Some(task) = self.task.as_mut() else {
      return (None, None);
    };
    task.held = false;
    // SAFETY: as in `take_mail`.
    let mailbox = unsafe { &mut *task.payload.cast::<Mailbox<T>>() };
    (mailbox.item.take(), mailbox.waker.take())
  }

  fn poll_impl<M, F>(payload: *mut (), cx: &mut Cx<'_>) -> bool
  where
    M: Drive,
    F: FnMut(M::Item) -> bool,
  {
    // SAFETY: `payload` was created from a `TaskPayload<M, F>` in `running`
    // and lives in the slot's arena. The task is marked busy, so this is the
    // only reference to it.
    let payload = unsafe { &mut *payload.cast::<TaskPayload<M, F>>() };
    loop {
      let Some(model) = payload.model.as_mut() else { return true };
      match model.drive(cx) {
        Turn::Pending => return false,
        Turn::Yield(item) => {
          let wanted = match payload.sink.as_mut() {
            Some(sink) => sink(item),
            None => false,
          };
          // The sink may also have dropped the consumer, which asks for
          // the model to stop: it must not take another step.
          if !wanted || cx.stopped() {
            payload.model = None;
            payload.sink = None;
            return true;
          }
        }
        Turn::End => {
          payload.model = None;
          payload.sink = None;
          return true;
        }
        Turn::Done(item) => {
          payload.model = None;
          // Dropped once it has run: a stream's consumer learns from that
          // drop that no further item is coming.
          if let Some(mut sink) = payload.sink.take() {
            sink(item);
          }
          return true;
        }
      }
    }
  }

  fn drop_task_impl<M, F>(payload: *mut ()) {
    // SAFETY: as in `poll_impl`. Taking both leaves nothing to drop twice.
    let payload = unsafe { &mut *payload.cast::<TaskPayload<M, F>>() };
    payload.model = None;
    payload.sink = None;
  }

  // --- the model ------------------------------------------------------------

  /// Marks the model busy and returns what polls it. `None` when there is
  /// no model left to poll, or it is busy already: then it is polled again
  /// when whoever has it is done.
  pub(crate) fn begin_poll(&mut self) -> Option<RawTask> {
    let task = self.task.as_mut()?;
    task.notified = false;
    if task.finished {
      return None;
    }
    if task.busy {
      task.woken = true;
      return None;
    }
    task.busy = true;
    task.woken = false;
    Some(RawTask {
      payload: task.payload,
      poll_fn: task.poll_fn,
      drop_fn: task.drop_fn,
    })
  }

  /// Ends a poll. Returns what to do next.
  pub(crate) fn end_poll(&mut self, finished: bool) -> AfterPoll {
    let Some(task) = self.task.as_mut() else {
      unreachable!("a poll ended in a slot that runs no model");
    };
    if finished {
      task.busy = false;
      task.finished = true;
      AfterPoll::Finished
    } else if task.cancelled {
      // Still busy: the caller drops the model, and then ends the poll
      // again with it finished.
      AfterPoll::Cancelled
    } else if task.woken {
      task.woken = false;
      AfterPoll::Again
    } else {
      task.busy = false;
      AfterPoll::Pending
    }
  }

  /// Asks for the model to be dropped. Returns what drops it now, or `None`
  /// when there is nothing to drop or whoever has the model will do it.
  pub(crate) fn cancel_task(&mut self) -> Option<RawTask> {
    let task = self.task.as_mut()?;
    if task.finished {
      return None;
    }
    task.cancelled = true;
    if task.busy {
      return None;
    }
    task.busy = true;
    Some(RawTask {
      payload: task.payload,
      poll_fn: task.poll_fn,
      drop_fn: task.drop_fn,
    })
  }

  /// A poll unwound. The model is in an unknown state: it is dropped, not
  /// polled again.
  pub(crate) fn poll_panicked(&mut self) {
    if let Some(task) = self.task.as_mut() {
      task.busy = false;
      task.cancelled = true;
    }
  }

  /// Whether this slot's model should be queued to be polled: it has one,
  /// that is not done and not queued yet.
  pub(crate) fn notify(&mut self) -> bool {
    match self.task.as_mut() {
      Some(task) if !task.finished && !task.notified => {
        task.notified = true;
        true
      }
      _ => false,
    }
  }

  /// Whether this slot's model was asked to stop and nobody has dropped it
  /// yet.
  pub(crate) fn is_cancelled(&self) -> bool {
    self.task.as_ref().is_some_and(|task| task.cancelled && !task.finished)
  }

  // --- the operation --------------------------------------------------------

  /// Whether this slot holds no operation, so that one may start. The
  /// caller also checks that no backend still holds the slot's lease.
  pub(crate) fn can_start_op(&self) -> bool {
    self.cell.stage == Stage::Free
  }

  /// Moves the memory of one operation into `arena` and records it as this
  /// slot's operation. Returns its sequence number.
  ///
  /// `arena` must be the slot's step arena, just reset: the memory lives
  /// until the next operation in this slot.
  pub(crate) fn start_op<S: 'static, Out: 'static>(
    &mut self,
    arena: &mut Bump,
    state: S,
    action: ActionFn<S>,
    complete: CompleteFn<S, Out>,
    waiter: u64,
  ) -> u64 {
    let payload: &mut OpPayload<S, Out> = arena.alloc_with(|| OpPayload {
      result: None,
      state: Some(state),
      action,
      complete,
    });
    self.occupy(
      (payload as *mut OpPayload<S, Out>).cast(),
      waiter,
      Self::with_action_impl::<S, Out>,
      Self::complete_impl::<S, Out>,
      Self::drop_op_impl::<S, Out>,
    )
  }

  /// [`start_op`](Self::start_op) for an operation that lends a buffer.
  ///
  /// The regions are made in `arena`, where they stay: they are a few
  /// hundred bytes, which would otherwise be copied in with `state`.
  pub(crate) fn start_lending_op<S: 'static, R: 'static, Out: 'static>(
    &mut self,
    arena: &mut Bump,
    state: S,
    action: LendFn<S, R>,
    complete: ReturnFn<S, R, Out>,
    waiter: u64,
  ) -> u64 {
    let payload = arena
      .alloc_layout(Layout::new::<LendingPayload<S, R, Out>>())
      .cast::<LendingPayload<S, R, Out>>()
      .as_ptr();
    // SAFETY: `payload` is a fresh allocation with the layout of a
    // `LendingPayload<S, R, Out>`. Every field is written here before
    // anything reads it.
    unsafe {
      (&raw mut (*payload).result).write(None);
      (&raw mut (*payload).state).write(Some(state));
      Regions::init(&raw mut (*payload).regions);
      (&raw mut (*payload).action).write(action);
      (&raw mut (*payload).complete).write(complete);
    }
    self.occupy(
      payload.cast(),
      waiter,
      Self::lend_impl::<S, R, Out>,
      Self::return_impl::<S, R, Out>,
      Self::drop_lending_impl::<S, R, Out>,
    )
  }

  /// Records `payload` as this slot's operation, in flight.
  fn occupy(
    &mut self,
    payload: *mut (),
    waiter: u64,
    with_action_fn: WithActionFn,
    complete_fn: fn(*mut (), Completion, bool),
    drop_fn: fn(*mut ()),
  ) -> u64 {
    assert!(
      self.cell.stage == Stage::Free,
      "driver: started an operation in a slot that holds one"
    );
    self.cell.seq += 1;
    self.cell.stage = Stage::InFlight;
    self.cell.abandoned = false;
    self.cell.waiter = waiter;
    self.cell.payload = payload;
    self.cell.with_action_fn = with_action_fn;
    self.cell.complete_fn = complete_fn;
    self.cell.drop_fn = drop_fn;
    self.cell.seq
  }

  fn lend_impl<S, R, Out>(
    payload: *mut (),
    f: &mut dyn for<'a> FnMut(Action<'a>),
  ) {
    // SAFETY: `payload` was created from a `LendingPayload<S, R, Out>` in
    // `start_lending_op` and stays valid until the slot's next operation.
    let payload = unsafe { &mut *payload.cast::<LendingPayload<S, R, Out>>() };
    let Some(state) = payload.state.as_mut() else {
      unreachable!("an operation was submitted after it was completed");
    };
    f((payload.action)(state, &mut payload.regions));
  }

  fn return_impl<S, R, Out>(
    payload: *mut (),
    completion: Completion,
    keep: bool,
  ) {
    // SAFETY: as in `lend_impl`. The operation is marked busy, so this is
    // the only reference to its memory.
    let payload = unsafe { &mut *payload.cast::<LendingPayload<S, R, Out>>() };
    let Some(state) = payload.state.as_mut() else {
      unreachable!("an operation was completed twice");
    };
    let out = (payload.complete)(state, &payload.regions, completion);
    // The backend is done with this memory.
    payload.state = None;
    if keep {
      payload.result = Some(out);
    }
  }

  fn drop_lending_impl<S, R, Out>(payload: *mut ()) {
    // SAFETY: as in `lend_impl`. Taking both leaves nothing to drop twice.
    // The regions are plain descriptions of memory and own nothing.
    let payload = unsafe { &mut *payload.cast::<LendingPayload<S, R, Out>>() };
    payload.state = None;
    payload.result = None;
  }

  fn with_action_impl<S, Out>(
    payload: *mut (),
    f: &mut dyn for<'a> FnMut(Action<'a>),
  ) {
    // SAFETY: `payload` was created from an `OpPayload<S, Out>` in
    // `start_op` and stays valid until the slot's next operation.
    let payload = unsafe { &mut *payload.cast::<OpPayload<S, Out>>() };
    let Some(state) = payload.state.as_mut() else {
      unreachable!("an operation was submitted after it was completed");
    };
    f((payload.action)(state));
  }

  fn complete_impl<S, Out>(
    payload: *mut (),
    completion: Completion,
    keep: bool,
  ) {
    // SAFETY: as in `with_action_impl`. The operation is marked busy, so
    // this is the only reference to its memory.
    let payload = unsafe { &mut *payload.cast::<OpPayload<S, Out>>() };
    let Some(state) = payload.state.as_mut() else {
      unreachable!("an operation was completed twice");
    };
    let out = (payload.complete)(state, completion);
    // The backend is done with this memory.
    payload.state = None;
    if keep {
      payload.result = Some(out);
    }
  }

  fn drop_op_impl<S, Out>(payload: *mut ()) {
    // SAFETY: as in `with_action_impl`. Taking both leaves nothing to drop
    // twice.
    let payload = unsafe { &mut *payload.cast::<OpPayload<S, Out>>() };
    payload.state = None;
    payload.result = None;
  }

  /// Runs `f` on the operation's action while its memory is borrowed.
  ///
  /// The driver calls this once, when it submits the operation.
  #[inline]
  pub(crate) fn with_action(&mut self, f: &mut dyn for<'a> FnMut(Action<'a>)) {
    (self.cell.with_action_fn)(self.cell.payload, f);
  }

  /// The operation could not be submitted: drops its memory and frees the
  /// slot's operation.
  pub(crate) fn abort_op(&mut self) {
    if self.cell.stage == Stage::InFlight {
      self.cell.stage = Stage::Free;
      (self.cell.drop_fn)(self.cell.payload);
    }
  }

  /// Marks the operation busy and returns what reads its result. `None`
  /// when no operation is in flight here.
  pub(crate) fn begin_completion(&mut self) -> Option<RawOp> {
    if self.cell.stage != Stage::InFlight {
      return None;
    }
    self.cell.stage = Stage::Busy;
    Some(self.raw_op())
  }

  /// The result was read. Returns the slot of the model to poll, when
  /// there is still a ticket to take the result.
  pub(crate) fn end_completion(&mut self) -> Option<u64> {
    if self.cell.abandoned {
      // Nothing is left to drop, unless the ticket was dropped while the
      // result was being read: then the result was kept for it.
      self.cell.stage = Stage::Free;
      (self.cell.drop_fn)(self.cell.payload);
      None
    } else {
      self.cell.stage = Stage::Ready;
      Some(self.cell.waiter)
    }
  }

  /// Reading the result unwound. The operation's memory is in an unknown
  /// state and is not dropped.
  pub(crate) fn completion_panicked(&mut self) {
    self.cell.stage = Stage::Free;
  }

  /// The result of operation `seq`, once, when it is in. Until then, the
  /// model in slot `waiter` is the one polled when it arrives.
  ///
  /// # Safety
  ///
  /// `Out` must be the result type operation `seq` of this slot was started
  /// with.
  pub(crate) unsafe fn take<Out>(
    &mut self,
    seq: u64,
    waiter: u64,
  ) -> Option<Out> {
    if self.cell.seq != seq {
      return None;
    }
    match self.cell.stage {
      Stage::Ready => {
        self.cell.stage = Stage::Free;
        // SAFETY: the payload is an `OpPayload<S, Out>` for the caller's
        // `Out`, which is `repr(C)` with `result: Option<Out>` first.
        unsafe { (*self.cell.payload.cast::<Option<Out>>()).take() }
      }
      Stage::InFlight => {
        self.cell.waiter = waiter;
        None
      }
      Stage::Free | Stage::Busy => None,
    }
  }

  /// Names the model to poll when the operation just started here is
  /// finished: the one that asked for its result while it was queued.
  pub(crate) fn set_waiter(&mut self, waiter: u64) {
    self.cell.waiter = waiter;
  }

  /// What dropping the ticket for operation `seq` asks for.
  pub(crate) fn abandon(&mut self, seq: u64) -> Abandon {
    if self.cell.seq != seq {
      return Abandon::Stale;
    }
    match self.cell.stage {
      Stage::InFlight => Abandon::InFlight,
      Stage::Ready => Abandon::Ready,
      // Its result is being read right now, for a ticket that is gone by
      // the time it is in: dropped then.
      Stage::Busy => {
        self.cell.abandoned = true;
        Abandon::Stale
      }
      Stage::Free => Abandon::Stale,
    }
  }

  /// The operation stays in the backend until it reports: its result is
  /// dropped when it does.
  pub(crate) fn abandon_in_flight(&mut self) {
    self.cell.abandoned = true;
  }

  /// Marks the operation busy and returns what drops its memory or its
  /// untaken result.
  pub(crate) fn begin_drop(&mut self) -> RawOp {
    self.cell.stage = Stage::Busy;
    self.raw_op()
  }

  /// The caller of [`begin_drop`](Self::begin_drop) is done.
  pub(crate) fn end_drop(&mut self) {
    self.cell.stage = Stage::Free;
  }

  fn raw_op(&self) -> RawOp {
    RawOp {
      payload: self.cell.payload,
      complete_fn: self.cell.complete_fn,
      drop_fn: self.cell.drop_fn,
      keep: !self.cell.abandoned,
    }
  }

  /// Nothing is left here: no model that is still running, and no
  /// operation. The slot can be given back.
  pub(crate) fn is_spent(&self) -> bool {
    // A model is marked finished only once whoever had it busy is done.
    self.cell.stage == Stage::Free
      && self.task.as_ref().is_none_or(|task| task.finished && !task.held)
  }

  /// The operation's action, with its borrow of the operation's memory
  /// erased, for a test to inspect.
  #[cfg(test)]
  pub(crate) fn action(&mut self) -> Option<Action<'static>> {
    let mut erased = None;
    self.with_action(&mut |action| {
      // SAFETY: tests inspect the action while this registration is alive
      // and never submit it.
      erased = Some(unsafe { action.erase_lifetime() });
    });
    erased
  }

  /// A slot holding one nop, in flight, whose memory holds `guard`.
  #[cfg(test)]
  pub(crate) fn nop_in<G: 'static>(arena: &mut Bump, guard: G) -> Self {
    let mut registration = Self::vacant();
    registration.start_op(
      arena,
      guard,
      |_| Action::nop(),
      |_, completion| completion.status(),
      0,
    );
    registration
  }
}

impl Drop for Registration {
  fn drop(&mut self) {
    // The operation first: its ticket, if the model still holds it, then
    // finds nothing to cancel.
    if self.cell.stage != Stage::Free {
      self.cell.stage = Stage::Free;
      (self.cell.drop_fn)(self.cell.payload);
    }
    // A finished model whose future never took its item still has it.
    if let Some(task) = self.task.take()
      && (!task.finished || task.held)
    {
      (task.drop_fn)(task.payload);
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::model::{OpModel, Step};
  use std::cell::Cell;
  use std::rc::Rc;

  /// Counts its drops.
  struct Counted(Rc<Cell<usize>>);

  impl Drop for Counted {
    fn drop(&mut self) {
      self.0.set(self.0.get() + 1);
    }
  }

  /// A slot holding one in-flight nop that reports the raw result and whose
  /// memory holds a `Counted`.
  fn counted(arena: &mut Bump, drops: &Rc<Cell<usize>>) -> (Registration, u64) {
    let mut registration = Registration::vacant();
    let seq = registration.start_op(
      arena,
      Counted(Rc::clone(drops)),
      |_| Action::nop(),
      |_, completion| completion.result(),
      7,
    );
    (registration, seq)
  }

  fn complete(reg: &mut Registration, result: isize) -> Option<u64> {
    let raw = reg.begin_completion().unwrap();
    assert!(reg.begin_completion().is_none(), "one completion at a time");
    // SAFETY: `reg` is alive and its operation was just marked busy.
    unsafe { raw.complete(Completion::new(result)) };
    reg.end_completion()
  }

  /// Done as soon as it is polled.
  struct Now;

  impl OpModel for Now {
    type Item = ();

    fn poll(&mut self, _: &mut Cx<'_>) -> Step<()> {
      Step::Done(())
    }
  }

  #[test]
  fn a_model_is_queued_once_until_it_is_polled_and_never_once_done() {
    assert!(!Registration::vacant().notify(), "no model to poll");

    let mut arena = Bump::new();
    let mut reg = Registration::running(&mut arena, Now, |()| true);
    assert!(!reg.is_spent(), "its model has not run");
    assert!(reg.notify());
    assert!(!reg.notify(), "queued already");

    assert!(reg.begin_poll().is_some());
    assert!(reg.notify(), "polled since: it can be queued again");
    assert!(matches!(reg.end_poll(true), AfterPoll::Finished));
    assert!(reg.is_spent());
    assert!(reg.begin_poll().is_none());
    assert!(!reg.notify(), "done");
  }

  #[test]
  fn a_result_whose_ticket_went_while_it_was_read_is_dropped() {
    fn held(guard: &mut Option<Counted>, _: Completion) -> Option<Counted> {
      guard.take()
    }
    let drops = Rc::new(Cell::new(0));
    let mut arena = Bump::new();
    let mut reg = Registration::vacant();
    let seq = reg.start_op(
      &mut arena,
      Some(Counted(Rc::clone(&drops))),
      |_| Action::nop(),
      held,
      0,
    );
    let raw = reg.begin_completion().unwrap();
    // SAFETY: `reg` is alive and its operation was just marked busy.
    unsafe { raw.complete(Completion::new(0)) };
    assert_eq!(drops.get(), 0, "the result holds the guard");

    assert!(matches!(reg.abandon(seq), Abandon::Stale), "nothing to cancel");
    assert_eq!(reg.end_completion(), None, "nobody to poll");
    assert_eq!(drops.get(), 1);
    assert!(reg.is_spent());
  }

  #[test]
  #[cfg_attr(miri, ignore = "leaks the operation's memory on purpose")]
  fn an_operation_whose_result_could_not_be_read_frees_its_slot() {
    let drops = Rc::new(Cell::new(0));
    let mut arena = Bump::new();
    let (mut reg, seq) = counted(&mut arena, &drops);
    assert!(reg.begin_completion().is_some());
    reg.completion_panicked();
    assert!(reg.can_start_op());
    assert!(reg.is_spent());
    // SAFETY: the operation was started with `isize` results.
    assert_eq!(unsafe { reg.take::<isize>(seq, 0) }, None);
    assert_eq!(drops.get(), 0, "memory in an unknown state is not dropped");
  }

  #[test]
  fn the_action_is_the_one_the_memory_builds() {
    let mut arena = Bump::new();
    let mut reg = Registration::nop_in(&mut arena, ());
    let mut seen = 0;
    reg.with_action(&mut |action| {
      assert!(action.is_nop());
      seen += 1;
    });
    assert_eq!(seen, 1);
    assert!(reg.action().is_some_and(|action| action.is_nop()));
  }

  #[test]
  fn a_result_waits_for_its_ticket_and_frees_the_slot_when_taken() {
    let drops = Rc::new(Cell::new(0));
    let mut arena = Bump::new();
    let (mut reg, seq) = counted(&mut arena, &drops);
    assert!(!reg.can_start_op());
    assert!(!reg.is_spent());
    // SAFETY: the operation was started with `isize` results.
    assert_eq!(unsafe { reg.take::<isize>(seq, 9) }, None, "not finished");

    assert_eq!(complete(&mut reg, 42), Some(9), "the last asker is polled");
    assert_eq!(drops.get(), 1, "the memory is dropped once the result is read");
    assert!(!reg.is_spent(), "the result is still here");

    // SAFETY: as above.
    assert_eq!(unsafe { reg.take::<isize>(seq + 1, 9) }, None, "another op");
    // SAFETY: as above.
    assert_eq!(unsafe { reg.take::<isize>(seq, 9) }, Some(42));
    // SAFETY: as above.
    assert_eq!(unsafe { reg.take::<isize>(seq, 9) }, None, "taken once");
    assert!(reg.can_start_op());
    assert!(reg.is_spent());
    assert_eq!(drops.get(), 1);
  }

  #[test]
  fn an_abandoned_operation_s_result_is_dropped_when_it_arrives() {
    let drops = Rc::new(Cell::new(0));
    let mut arena = Bump::new();
    let (mut reg, seq) = counted(&mut arena, &drops);
    assert!(matches!(reg.abandon(seq + 1), Abandon::Stale));
    assert!(matches!(reg.abandon(seq), Abandon::InFlight));
    reg.abandon_in_flight();
    assert!(!reg.is_spent(), "the backend still has it");

    assert_eq!(complete(&mut reg, 1), None, "nobody to poll");
    assert!(reg.is_spent());
    assert_eq!(drops.get(), 1);
  }

  #[test]
  fn an_untaken_result_can_be_dropped() {
    fn held(guard: &mut Option<Counted>, _: Completion) -> Option<Counted> {
      guard.take()
    }
    let drops = Rc::new(Cell::new(0));
    let mut arena = Bump::new();
    let mut reg = Registration::vacant();
    let seq = reg.start_op(
      &mut arena,
      Some(Counted(Rc::clone(&drops))),
      |_| Action::nop(),
      held,
      0,
    );
    assert_eq!(complete(&mut reg, 0), Some(0));
    assert_eq!(drops.get(), 0, "the result holds the guard now");

    assert!(matches!(reg.abandon(seq), Abandon::Ready));
    let raw = reg.begin_drop();
    assert!(!reg.can_start_op(), "busy while it is dropped");
    // SAFETY: `reg` is alive and its operation was just marked busy.
    unsafe { raw.drop_payload() };
    reg.end_drop();
    assert!(reg.is_spent());
    assert_eq!(drops.get(), 1);
  }

  #[test]
  fn dropping_a_slot_drops_what_its_operation_holds() {
    let drops = Rc::new(Cell::new(0));
    let mut arena = Bump::new();
    let (reg, _) = counted(&mut arena, &drops);
    drop(reg);
    assert_eq!(drops.get(), 1);

    let mut reg = Registration::nop_in(&mut arena, Counted(Rc::clone(&drops)));
    reg.abort_op();
    assert_eq!(drops.get(), 2, "an operation that was not submitted");
    assert!(reg.is_spent());
    drop(reg);
    assert_eq!(drops.get(), 2);
  }

  #[test]
  fn a_slot_s_operations_are_told_apart_by_sequence() {
    let mut arena = Bump::new();
    let mut reg = Registration::vacant();
    let first =
      reg.start_op(&mut arena, (), |_| Action::nop(), |_, c| c.result(), 0);
    assert_eq!(complete(&mut reg, 1), Some(0));
    // SAFETY: the operation was started with `isize` results.
    assert_eq!(unsafe { reg.take::<isize>(first, 0) }, Some(1));

    let second =
      reg.start_op(&mut arena, (), |_| Action::nop(), |_, c| c.result(), 0);
    assert_ne!(first, second);
    assert!(matches!(reg.abandon(first), Abandon::Stale));
    assert!(matches!(reg.abandon(second), Abandon::InFlight));
  }
}
