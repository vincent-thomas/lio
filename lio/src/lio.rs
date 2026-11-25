use crate::{
  api::op::{Action, Completion},
  backend::{
    IoBackend, OpCompleted,
    store::{Detached, OpStore},
  },
  model::{Cx, OpModel},
  registration::{Abandon, AfterPoll, RawTask, Registration},
  time::TimeManager,
};

use std::{
  cell::{Cell, RefCell, RefMut},
  collections::VecDeque,
  io,
  mem::ManuallyDrop,
  rc::{Rc, Weak},
  task::Waker,
  time::{Duration, Instant},
};

use bumpalo::Bump;

thread_local! {
  static GLOBAL_LIO: RefCell<Option<Lio>> = const { RefCell::new(None) };
}

#[cfg(test)]
thread_local! {
  static STORE_DROPPED: std::cell::Cell<bool> =
    const { std::cell::Cell::new(false) };
}

/// Guard returned by [`install_global`].
///
/// Dropping the guard uninstalls the thread-local global [`Lio`] it
/// installed. If that one was already uninstalled, and possibly replaced by
/// another, the guard leaves the global alone.
#[must_use = "Dropping the guard immediately uninstalls the global Lio"]
pub struct GlobalLioGuard {
  /// The driver this guard installed, without keeping it alive.
  installed: Weak<RefCell<LioInner>>,
}

impl Drop for GlobalLioGuard {
  fn drop(&mut self) {
    // The thread-local may already be gone if this runs during thread exit.
    let ours = GLOBAL_LIO.try_with(|global| {
      let mut global = global.borrow_mut();
      let installed = &self.installed;
      if global.as_ref().is_some_and(|lio| {
        std::ptr::eq(Rc::as_ptr(&lio.inner), installed.as_ptr())
      }) {
        global.take()
      } else {
        None
      }
    });
    // Dropped outside the thread-local's borrow: if this was the last
    // handle, tearing the driver down runs user code that may look it up.
    drop(ours);
  }
}

/// Installs a global Lio instance for the current thread.
///
/// After calling this, operations can be used without explicitly calling `.with_lio()`.
/// This is useful for thread-per-core designs where each thread has its own Lio instance.
///
/// Returns a guard that uninstalls the global driver when dropped.
///
/// # Panics
///
/// Panics if a global Lio is already installed on this thread.
pub fn install_global(lio: Lio) -> GlobalLioGuard {
  GLOBAL_LIO.with(|global| {
    let mut global = global.borrow_mut();
    if global.is_some() {
      panic!(
        "Global Lio already installed on this thread. Drop the existing GlobalLioGuard or call uninstall_global() first."
      );
    }
    let installed = Rc::downgrade(&lio.inner);
    *global = Some(lio);
    GlobalLioGuard { installed }
  })
}

/// Uninstalls the global Lio instance for the current thread.
///
/// Returns the previously installed Lio, or `None` if no global was installed.
pub fn uninstall_global() -> Option<Lio> {
  GLOBAL_LIO.with(|global| global.borrow_mut().take())
}

/// Returns a clone of the global Lio instance for the current thread.
///
/// Returns `None` if no global Lio has been installed.
pub(crate) fn get_global() -> Option<Lio> {
  GLOBAL_LIO.with(|global| global.borrow().clone())
}

struct LioInner {
  // Dropped by hand in `Drop`: the backend must drain in-flight work before
  // `store` frees the memory that work points into, and both are leaked
  // when it cannot.
  io: ManuallyDrop<Box<dyn IoBackend>>,
  store: ManuallyDrop<OpStore>,
  /// The backend's close for resources its completions carry.
  resource_close: crate::api::resource::ResourceClose,
  time: TimeManager,
  /// Reused buffer the backend reports completions into.
  completed: Vec<OpCompleted>,
  /// Completions and expired timers waiting to be routed, in order.
  ///
  /// They live here rather than in the run that collected them so that a
  /// nested run (from a handler) routes the rest instead of waiting on the
  /// backend, and so that what a panicking handler left behind is simply
  /// still queued for the next run.
  ready_io: VecDeque<OpCompleted>,
  ready_timers: VecDeque<u64>,
  /// Registrations removed while the driver was borrowed, to be dropped
  /// once it is not (see [`Lio::bury`]). Their slots stay reserved until then.
  dying: Vec<Detached>,
  /// Slot indices reused across [`Lio::bury_and_wake`] so releasing a batch
  /// does not allocate.
  released_slots: Vec<u32>,
  /// Registrations taken from `dying` and being dropped right now; their
  /// slots are released once they are gone.
  burying: usize,
  /// Slots retired while a backend still held their lease. The registration
  /// is leaked, and the slot is never released, so the buffers stay valid.
  leaked: usize,
  /// Submissions made while every slot was taken, scheduled in order as
  /// slots free up. Only allocates when the driver is over capacity.
  overflow: VecDeque<Queued>,
  /// Queued submissions whose consumer was dropped before a slot freed up.
  /// They hold user values, so they are dropped once the driver is not
  /// borrowed.
  abandoned: Vec<Queued>,
  /// Slots whose model has something to do: it was just started from the
  /// queue, or one of its operations finished. Polled once the driver is
  /// not borrowed.
  woken: VecDeque<u64>,
  /// Futures waiting for a free slot.
  capacity_waiters: Vec<Waker>,
  profile: Option<LioProfile>,
}

/// Starts what was waiting for a slot. Runs with the driver borrowed.
///
/// Given `None` it takes a free slot. Given the slot of the model that
/// started it, it uses that slot, which must be able to hold an operation.
/// Returns the slot it is in and its sequence number there.
type PendingSubmission =
  Box<dyn FnOnce(&mut LioInner, Option<u64>) -> (u64, u64)>;

/// A submission waiting for a slot.
struct Queued {
  init: PendingSubmission,
  /// Shared with a consumer that may give up before a slot frees up.
  token: Option<Rc<QueueToken>>,
  /// For an operation: the slot of the model that started it. It can run
  /// there too, once that slot's own operation is out of the way, so a
  /// model never waits on a slot only it could free.
  home: Option<u64>,
}

/// Where a model that was asked to run is.
pub(crate) enum Spawned {
  /// In this slot.
  Now(u64),
  /// Waiting for a slot.
  Queued(Rc<QueueToken>),
}

/// Moves the memory of one operation into a slot's step arena and records
/// it as that slot's operation, for the model in the given slot. Returns
/// the operation's sequence number there.
pub(crate) trait PlaceOp:
  FnOnce(&mut Registration, &mut Bump, u64) -> u64 + 'static
{
}

impl<F> PlaceOp for F where
  F: FnOnce(&mut Registration, &mut Bump, u64) -> u64 + 'static
{
}

/// What to do with a model that was marked busy.
enum Begun {
  Poll(RawTask),
  /// It was asked to stop: drop it.
  Drop(RawTask),
}

/// The operation a ticket stands for.
pub(crate) enum OpRef {
  /// Operation `seq` of slot `id`.
  At { id: u64, seq: u64 },
  /// Waiting for a slot of its own, where it will be the first operation.
  Queued(Rc<QueueToken>),
}

/// The sequence number of a slot's first operation.
const FIRST_OP: u64 = 1;

/// What a queued submission and its consumer tell each other.
#[derive(Default)]
pub(crate) struct QueueToken {
  /// Set by the consumer: do not submit this.
  abandoned: Cell<bool>,
  /// Set by the driver once the submission has a slot: the slot, and its
  /// sequence number there.
  at: Cell<Option<(u64, u64)>>,
}

impl OpRef {
  /// The slot and sequence number, once the operation has a slot.
  fn resolved(&self) -> Option<(u64, u64)> {
    match self {
      OpRef::At { id, seq } => Some((*id, *seq)),
      OpRef::Queued(token) => token.at.get(),
    }
  }
}

impl QueueToken {
  /// The slot it is in, once it has left the queue.
  pub(crate) fn id(&self) -> Option<u64> {
    self.at.get().map(|(id, _)| id)
  }

  /// Keeps the submission from ever reaching the backend. Only has an
  /// effect while it is still queued.
  pub(crate) fn abandon(&self) {
    self.abandoned.set(true);
  }
}

#[derive(Debug, Default)]
struct LioProfile {
  run_inner_calls: usize,
  completions_returned: usize,
  stale_completions: usize,
  timer_expirations: usize,
  remove_count: usize,
  flush_time: Duration,
  next_deadline_time: Duration,
  wait_time: Duration,
  completion_loop_time: Duration,
  completion_store_lookup_time: Duration,
  completion_on_completion_time: Duration,
  completion_is_finished_time: Duration,
  timer_poll_time: Duration,
  timer_loop_time: Duration,
  remove_time: Duration,
}

impl LioProfile {
  fn count_timer_expirations(&mut self, before: usize, after: usize) {
    self.timer_expirations += after - before;
  }

  fn count_stale_completion(&mut self) {
    self.stale_completions += 1;
  }

  fn count_remove(&mut self) {
    self.remove_count += 1;
  }

  fn end_completion_loop(&mut self, started: Option<Instant>) {
    if let Some(started) = started {
      self.completion_loop_time += started.elapsed();
    }
  }

  fn merge(&mut self, run: &LioProfile) {
    self.run_inner_calls += 1;
    self.completions_returned += run.completions_returned;
    self.stale_completions += run.stale_completions;
    self.timer_expirations += run.timer_expirations;
    self.remove_count += run.remove_count;
    self.flush_time += run.flush_time;
    self.next_deadline_time += run.next_deadline_time;
    self.wait_time += run.wait_time;
    self.completion_loop_time += run.completion_loop_time;
    self.completion_store_lookup_time += run.completion_store_lookup_time;
    self.completion_on_completion_time += run.completion_on_completion_time;
    self.completion_is_finished_time += run.completion_is_finished_time;
    self.timer_poll_time += run.timer_poll_time;
    self.timer_loop_time += run.timer_loop_time;
    self.remove_time += run.remove_time;
  }
}

/// Times `$body` into `$stats.$field` when `$profile` (a const) is set.
macro_rules! timed {
  ($profile:expr, $stats:expr, $field:ident, $body:expr) => {{
    if $profile {
      let started = Instant::now();
      let value = $body;
      $stats.$field += started.elapsed();
      value
    } else {
      $body
    }
  }};
}

impl Drop for LioInner {
  fn drop(&mut self) {
    // They point into the store's arenas, so they go before it.
    drop(std::mem::take(&mut self.dying));
    if self.io.drain() {
      // SAFETY: the backend is dropped first, so it drops the leases it
      // stored. A model whose lease is still held stays leaked with the store.
      unsafe { ManuallyDrop::drop(&mut self.io) };
      // `leaked` counts registrations forgotten while a backend still held
      // their lease. Their arenas live in the store, so the store stays too.
      if self.leaked == 0 {
        #[cfg(test)]
        STORE_DROPPED.with(|dropped| dropped.set(true));
        // SAFETY: every registration was dropped, and this field is not
        // used again.
        unsafe { ManuallyDrop::drop(&mut self.store) };
      }
    }
    // Otherwise an operation may still be running in the kernel or on a
    // backend thread, writing into buffers and lowered state owned by the
    // store. Leaking both keeps that memory valid, and keeps the backend
    // from waiting again in its own `Drop`.

    let Some(profile) = &self.profile else {
      return;
    };
    eprintln!(
      "\nlio-profile run_inner_calls={} completions_returned={} stale_completions={} timer_expirations={} remove_count={} flush_ms={:.3} next_deadline_ms={:.3} wait_ms={:.3} completion_loop_ms={:.3} completion_store_lookup_ms={:.3} completion_on_completion_ms={:.3} completion_is_finished_ms={:.3} timer_poll_ms={:.3} timer_loop_ms={:.3} remove_ms={:.3}",
      profile.run_inner_calls,
      profile.completions_returned,
      profile.stale_completions,
      profile.timer_expirations,
      profile.remove_count,
      profile.flush_time.as_secs_f64() * 1000.0,
      profile.next_deadline_time.as_secs_f64() * 1000.0,
      profile.wait_time.as_secs_f64() * 1000.0,
      profile.completion_loop_time.as_secs_f64() * 1000.0,
      profile.completion_store_lookup_time.as_secs_f64() * 1000.0,
      profile.completion_on_completion_time.as_secs_f64() * 1000.0,
      profile.completion_is_finished_time.as_secs_f64() * 1000.0,
      profile.timer_poll_time.as_secs_f64() * 1000.0,
      profile.timer_loop_time.as_secs_f64() * 1000.0,
      profile.remove_time.as_secs_f64() * 1000.0,
    );
  }
}

#[derive(Clone)]
pub struct Lio {
  inner: Rc<RefCell<LioInner>>,
}

/// A handle that does not keep the driver alive.
///
/// What the driver itself stores (a model waiting on its operations) holds
/// this, so dropping the last [`Lio`] still drops the driver.
#[derive(Clone)]
pub(crate) struct WeakLio {
  inner: Weak<RefCell<LioInner>>,
}

impl WeakLio {
  /// Whether this is a handle to `lio`.
  pub(crate) fn is(&self, lio: &Lio) -> bool {
    std::ptr::eq(self.inner.as_ptr(), Rc::as_ptr(&lio.inner))
  }

  pub(crate) fn upgrade(&self) -> Option<Lio> {
    self.inner.upgrade().map(|inner| Lio { inner })
  }
}

impl Lio {
  pub(crate) fn downgrade(&self) -> WeakLio {
    WeakLio { inner: Rc::downgrade(&self.inner) }
  }

  pub(crate) fn pause_time(&self) {
    self.inner.borrow_mut().time.pause();
  }

  pub(crate) fn resume_time(&self) {
    self.inner.borrow_mut().time.resume();
  }

  pub(crate) fn advance_time_by_ticks(&self, ticks: u64) {
    self.inner.borrow_mut().time.advance_by_ticks(ticks);
  }

  /// Creates a new Lio driver with the default backend and specified capacity.
  ///
  /// # Arguments
  ///
  /// * `cap` - The number of operations that can be in flight at once
  ///
  /// All memory for `cap` operations is reserved up front, so a driver sized
  /// for its peak load does not allocate per operation. Beyond `cap`, futures
  /// wait for a free slot, while [`Io::when_done`](crate::api::Io::when_done)
  /// and [`Io::send`](crate::api::Io::send) queue the operation, which
  /// allocates; see [`Lio::is_full`].
  ///
  /// # Errors
  ///
  /// As for [`Lio::new_with_backend`]; in particular `cap` must not be zero.
  ///
  /// # Example
  ///
  /// ```
  /// use lio::Lio;
  ///
  /// let mut lio = Lio::new(1024).unwrap();
  /// ```
  #[cfg(feature = "backend_impls")]
  pub fn new(cap: usize) -> io::Result<Self> {
    Self::new_with_backend(crate::backend::impls::DefaultBackend::new(), cap)
  }

  /// Creates a new Lio driver with the specified backend and capacity.
  ///
  /// # Arguments
  ///
  /// * `backend` - The I/O backend implementation (e.g., io_uring, epoll/kqueue)
  /// * `cap` - The number of operations that can be in flight at once; see
  ///   [`Lio::new`]
  ///
  /// # Errors
  ///
  /// [`io::ErrorKind::InvalidInput`] if `cap` is zero: such a driver could
  /// never run an operation. Otherwise, whatever initializing the backend
  /// reports.
  pub fn new_with_backend<D>(mut backend: D, cap: usize) -> io::Result<Self>
  where
    D: IoBackend + 'static,
  {
    if cap == 0 {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "lio capacity must be at least 1",
      ));
    }
    backend.init(cap)?;

    let inner = LioInner {
      resource_close: backend.resource_close(),
      io: ManuallyDrop::new(Box::new(backend)),
      store: ManuallyDrop::new(OpStore::with_capacity(cap)),
      time: TimeManager::with_capacity(cap),
      completed: Vec::with_capacity(cap),
      ready_io: VecDeque::with_capacity(cap),
      ready_timers: VecDeque::new(),
      dying: Vec::with_capacity(cap),
      released_slots: Vec::with_capacity(cap),
      burying: 0,
      leaked: 0,
      overflow: VecDeque::new(),
      abandoned: Vec::new(),
      woken: VecDeque::new(),
      capacity_waiters: Vec::new(),
      profile: std::env::var_os("LIO_PROFILE").map(|_| LioProfile::default()),
    };
    Ok(Self { inner: Rc::new(RefCell::new(inner)) })
  }

  /// Runs `model`, passing every item it produces to `sink` until the model
  /// is done or `sink` returns `false`.
  ///
  /// The model moves into a slot and is polled for the first time here.
  /// Over capacity it waits in a queue (see [`Lio::is_full`]).
  pub(crate) fn spawn<M, F>(&self, model: M, sink: F) -> Spawned
  where
    M: OpModel + 'static,
    F: FnMut(M::Item) -> bool + 'static,
  {
    let mut inner = self.inner.borrow_mut();
    // Queued submissions go first.
    if !inner.overflow.is_empty() || inner.store.is_full() {
      let token = Rc::new(QueueToken::default());
      let start = move |inner: &mut LioInner, _home: Option<u64>| {
        let id = Self::insert_task(inner, model, sink);
        Self::wake(inner, id);
        (id, FIRST_OP)
      };
      Self::enqueue(&mut inner, start, Some(Rc::clone(&token)), None);
      return Spawned::Queued(token);
    }
    let id = Self::insert_task(&mut inner, model, sink);
    let begun = Self::begin(&mut inner, id);
    let Some(begun) = begun else {
      unreachable!("driver: a model that was just started cannot be polled");
    };
    let (inner, freed) = self.poll_begun(inner, id, begun);
    drop(inner);
    // It may be done already.
    if freed {
      self.bury_and_wake();
    }
    Spawned::Now(id)
  }

  /// Stops a model started with [`spawn`](Self::spawn): it is dropped, and
  /// what it has in flight is cancelled.
  pub(crate) fn cancel(&self, spawned: &Spawned) {
    let id = match spawned {
      Spawned::Now(id) => Some(*id),
      Spawned::Queued(token) => {
        // Still queued: it never starts.
        token.abandon();
        token.id()
      }
    };
    if let Some(id) = id {
      self.cancel_task(id);
    }
  }

  /// Moves a model into a free slot. The store must have one.
  fn insert_task<M, F>(inner: &mut LioInner, model: M, sink: F) -> u64
  where
    M: OpModel + 'static,
    F: FnMut(M::Item) -> bool + 'static,
  {
    let inserted = inner.store.insert_submitting(
      |arena| Registration::running(arena, model, sink),
      |_id, _reg, _step_bump, _lease, insert| insert.keep_on_panic(),
    );
    let Some(id) = inserted else {
      unreachable!("driver: a slot that was kept was not claimed");
    };
    id
  }

  /// Queues the model in slot `id` to be polled, unless it already is.
  fn wake(inner: &mut LioInner, id: u64) {
    if inner.store.get_mut(id).is_some_and(|reg| reg.notify()) {
      inner.woken.push_back(id);
    }
  }

  /// Polls the models that have something to do.
  fn poll_woken(&self) {
    let mut inner = self.inner.borrow_mut();
    loop {
      let mut freed = false;
      // One at a time: a poll may run the driver, which polls the rest.
      while let Some(id) = inner.woken.pop_front() {
        if let Some(begun) = Self::begin(&mut inner, id) {
          let polled;
          (inner, polled) = self.poll_begun(inner, id, begun);
          freed |= polled;
        }
      }
      if !freed {
        return;
      }
      // Give back the slots of the models that finished, in one go. That
      // may start models that were waiting for one.
      drop(inner);
      self.bury_and_wake();
      inner = self.inner.borrow_mut();
    }
  }

  /// Marks the model in slot `id` busy and returns what polls it, or what
  /// drops it if it was asked to stop while it could not be dropped.
  fn begin(inner: &mut LioInner, id: u64) -> Option<Begun> {
    let reg = inner.store.get_mut(id)?;
    if reg.is_cancelled() {
      return reg.cancel_task().map(Begun::Drop);
    }
    reg.begin_poll().map(Begun::Poll)
  }

  /// Polls the model in slot `id`, which [`begin`](Self::begin) marked
  /// busy, until it has nothing more to do. The driver is not borrowed
  /// while the model runs: it starts operations through it.
  ///
  /// Returns the borrow, taken again, and whether the slot was emptied: it
  /// is given back by the next [`bury_and_wake`](Self::bury_and_wake).
  fn poll_begun<'a>(
    &'a self,
    mut inner: RefMut<'a, LioInner>,
    id: u64,
    begun: Begun,
  ) -> (RefMut<'a, LioInner>, bool) {
    let (raw, mut dropping) = match begun {
      Begun::Poll(raw) => (raw, false),
      Begun::Drop(raw) => (raw, true),
    };
    loop {
      drop(inner);
      let finished = if dropping {
        // SAFETY: the registration is alive and marked busy, which keeps
        // its slot from being given back. Dropped with the driver not
        // borrowed: dropping the model drops its tickets, which cancel.
        unsafe { raw.drop_model() };
        true
      } else {
        let mut guard = PollGuard { lio: self, id, armed: true };
        // SAFETY: as above.
        let finished = unsafe { raw.poll(&mut Cx::new(self, id)) };
        guard.armed = false;
        finished
      };

      inner = self.inner.borrow_mut();
      let inner_ref = &mut *inner;
      let Some(reg) = inner_ref.store.get_mut(id) else {
        panic!("driver: slot {id} was given back while its model was busy");
      };
      match reg.end_poll(finished) {
        AfterPoll::Again => {}
        AfterPoll::Cancelled => dropping = true,
        AfterPoll::Pending => return (inner, false),
        AfterPoll::Finished => {
          let spent = reg.is_spent();
          if spent {
            Self::retire(inner_ref, id);
          }
          return (inner, spent);
        }
      }
    }
  }

  /// Whether the model in slot `id` was asked to stop and is not dropped
  /// yet.
  pub(crate) fn is_stopped(&self, id: u64) -> bool {
    let mut inner = self.inner.borrow_mut();
    inner.store.get_mut(id).is_some_and(|reg| reg.is_cancelled())
  }

  /// Drops the model in slot `id`. Its consumer is gone.
  fn cancel_task(&self, id: u64) {
    let mut inner = self.inner.borrow_mut();
    let Some(raw) = inner.store.get_mut(id).and_then(|reg| reg.cancel_task())
    else {
      return;
    };
    let (inner, freed) = self.poll_begun(inner, id, Begun::Drop(raw));
    drop(inner);
    if freed {
      self.bury_and_wake();
    }
  }

  /// Starts an operation for the model in slot `task`: moves `state`, the
  /// memory the operation uses, into a slot, and submits what `action`
  /// builds from it. `complete` reads the result out of that memory.
  ///
  /// The operation goes into the model's own slot when that holds none, and
  /// otherwise into a slot of its own, waiting for one over capacity.
  pub(crate) fn submit_op(&self, task: u64, place: impl PlaceOp) -> OpRef {
    let mut inner = self.inner.borrow_mut();
    let place = match Self::start_in_slot(&mut inner, task, place) {
      Ok(seq) => return OpRef::At { id: task, seq },
      Err(place) => place,
    };
    if !inner.overflow.is_empty() || inner.store.is_full() {
      let token = Rc::new(QueueToken::default());
      let start = move |inner: &mut LioInner, home: Option<u64>| {
        let Some(home) = home else {
          let id = Self::insert_op(inner, task, place);
          return (id, FIRST_OP);
        };
        match Self::start_in_slot(inner, home, place) {
          Ok(seq) => (home, seq),
          Err(_) => unreachable!(
            "driver: a queued operation was sent to a slot that holds one"
          ),
        }
      };
      Self::enqueue(&mut inner, start, Some(Rc::clone(&token)), Some(task));
      return OpRef::Queued(token);
    }
    let id = Self::insert_op(&mut inner, task, place);
    OpRef::At { id, seq: FIRST_OP }
  }

  /// Starts an operation in slot `id`, which is taken already. Gives
  /// `state` back when that slot cannot hold another operation right now.
  fn start_in_slot<P: PlaceOp>(
    inner: &mut LioInner,
    id: u64,
    place: P,
  ) -> Result<u64, P> {
    let LioInner { io, store, time, .. } = inner;
    let started = store.with_slot(id, move |reg, step_bump, lease| {
      // A backend that still holds the lease may still use the memory of
      // the operation before this one.
      if !reg.can_start_op() || lease.held_elsewhere() {
        return Err(place);
      }
      // The operation before this one is finished and its result taken:
      // nothing points into the arena any more.
      step_bump.reset();
      let seq = place(reg, step_bump, id);
      let keep = lease.clone();
      let mut abort = AbortOp { reg, armed: true };
      let AbortOp { reg, armed } = &mut abort;
      reg.with_action(&mut |action| {
        // After this, a panic must not free the operation's memory: `push`
        // may have copied its addresses. A panic while building the action
        // happens first, and frees it.
        *armed = false;
        match action {
          Action::Io(op) => io.push(
            id,
            crate::backend::InFlight::from_model(op, keep.clone()),
            step_bump,
          ),
          Action::Sleep(duration) => time.schedule(id, duration),
        }
      });
      Ok(seq)
    });
    let Some(started) = started else {
      unreachable!("driver: slot {id} is gone while its model starts work");
    };
    started
  }

  /// Starts an operation in a slot of its own, for the model in slot
  /// `waiter`. The store must have a free slot.
  fn insert_op(inner: &mut LioInner, waiter: u64, place: impl PlaceOp) -> u64 {
    let LioInner { io, store, time, .. } = inner;
    let inserted = store.insert_submitting(
      |_arena| Registration::vacant(),
      |id, reg, step_bump, lease, insert| {
        place(reg, step_bump, waiter);
        let keep = lease.clone();
        reg.with_action(&mut |action| {
          // After this, a panic must not free the operation's memory:
          // `push` may have copied its addresses. A panic while building
          // the action happens first and still frees the slot.
          insert.keep_on_panic();
          match action {
            Action::Io(op) => io.push(
              id,
              crate::backend::InFlight::from_model(op, keep.clone()),
              step_bump,
            ),
            Action::Sleep(duration) => time.schedule(id, duration),
          }
        });
      },
    );
    let Some(id) = inserted else {
      unreachable!("driver: an operation was submitted and its slot not kept");
    };
    id
  }

  /// The result of the operation `op` stands for, once, when it is in.
  /// Until then, the model in slot `waiter` is the one polled when it
  /// arrives.
  ///
  /// # Safety
  ///
  /// `op` must have come from a [`submit_op`](Self::submit_op) on this
  /// driver whose `complete` returns `Out`.
  pub(crate) unsafe fn take_op<Out>(
    &self,
    op: &OpRef,
    waiter: u64,
  ) -> Option<Out> {
    let (id, seq) = op.resolved()?;
    let mut inner = self.inner.borrow_mut();
    let inner_ref = &mut *inner;
    let reg = inner_ref.store.get_mut(id)?;
    // SAFETY: a slot id is never reused with the same generation, and
    // `seq` is this slot's count of operations, so `(id, seq)` names the
    // one operation the caller's `submit_op` started, whose result is `Out`.
    let out = unsafe { reg.take::<Out>(seq, waiter) }?;
    let spent = reg.is_spent();
    if spent {
      Self::retire(inner_ref, id);
    } else {
      Self::start_waiting_at_home(inner_ref, id);
    }
    drop(inner);
    if spent {
      self.bury_and_wake();
    }
    Some(out)
  }

  /// The ticket for `op` is gone: nobody will take its result.
  ///
  /// An operation still in the backend is asked to finish early, and stays
  /// in its slot, with its memory, until it has.
  pub(crate) fn drop_op(&self, op: &OpRef) {
    if let OpRef::Queued(token) = op {
      // Still queued: it never reaches the backend.
      token.abandon();
    }
    let Some((id, seq)) = op.resolved() else { return };
    let raw = {
      let mut inner = self.inner.borrow_mut();
      let LioInner { io, store, time, ready_timers, .. } = &mut *inner;
      let Some(reg) = store.get_mut(id) else { return };
      match reg.abandon(seq) {
        Abandon::Stale => return,
        Abandon::Ready => reg.begin_drop(),
        Abandon::InFlight if time.remove(id) => {
          // A timer lives in the driver: nothing will report it. If it
          // fired already, its firing must not reach the slot's next
          // operation.
          ready_timers.retain(|&fired| fired != id);
          reg.begin_drop()
        }
        Abandon::InFlight => {
          reg.abandon_in_flight();
          io.cancel(id);
          return;
        }
      }
    };
    // SAFETY: the registration is alive and its operation was just marked
    // busy. Dropped with the driver not borrowed: this runs user code.
    unsafe { raw.drop_payload() };
    let spent = {
      let mut inner = self.inner.borrow_mut();
      let inner = &mut *inner;
      let Some(reg) = inner.store.get_mut(id) else { return };
      reg.end_drop();
      let spent = reg.is_spent();
      if spent {
        Self::retire(inner, id);
      } else {
        Self::start_waiting_at_home(inner, id);
      }
      spent
    };
    if spent {
      self.bury_and_wake();
    }
  }

  /// Queues a submission until a slot frees up. Kept out of line: it only
  /// runs when the driver is over capacity.
  #[cold]
  #[inline(never)]
  fn enqueue(
    inner: &mut LioInner,
    init: impl FnOnce(&mut LioInner, Option<u64>) -> (u64, u64) + 'static,
    token: Option<Rc<QueueToken>>,
    home: Option<u64>,
  ) {
    assert!(
      !inner.overflow.is_empty() || inner.store.is_full(),
      "driver: queued a submission while a slot is free"
    );
    inner.overflow.push_back(Queued { init: Box::new(init), token, home });
  }

  /// Slot `home` can hold an operation again: starts there the first
  /// operation its model has waiting for a slot, if any.
  fn start_waiting_at_home(inner: &mut LioInner, home: u64) {
    while !inner.overflow.is_empty()
      && inner.store.get_mut(home).is_some_and(|reg| reg.can_start_op())
      && !inner.store.lease_held(home)
    {
      let waiting =
        inner.overflow.iter().position(|queued| queued.home == Some(home));
      let Some(queued) = waiting.and_then(|at| inner.overflow.remove(at))
      else {
        return;
      };
      if queued.token.as_ref().is_some_and(|token| token.abandoned.get()) {
        inner.abandoned.push(queued);
        continue;
      }
      let Queued { init, token, .. } = queued;
      let at = init(inner, Some(home));
      if let Some(token) = token {
        token.at.set(Some(at));
      }
      return;
    }
  }

  /// Gives back slot `id`, which is spent.
  #[inline]
  fn retire(inner: &mut LioInner, id: u64) {
    assert!(
      !inner.time.contains(id),
      "driver: removed registration {id} with its timer still scheduled"
    );
    if inner.store.lease_held(id) {
      // A backend may still use the operation's memory: the registration
      // is leaked and the slot stays reserved, so that memory stays valid.
      let Some(detached) = inner.store.detach(id) else {
        unreachable!("driver: retired slot {id}, which is not taken");
      };
      inner.leaked += 1;
      std::mem::forget(detached);
    } else {
      // The slot is spent: its model and its consumer were dropped when the
      // model finished, and its operation's result was taken. Nothing is
      // left that would run user code, so it is dropped where it is and the
      // slot goes back right away.
      let removed = inner.store.remove_in_place(id);
      assert!(removed, "driver: retired slot {id}, which is not taken");
    }
    Self::assert_retired_slots_tracked(inner);
  }

  /// Every retired slot belongs to a registration waiting in `dying` or
  /// being dropped by [`bury_and_wake`](Self::bury_and_wake). One that
  /// belongs to neither was detached without being queued for release, and
  /// is never reused.
  #[inline]
  fn assert_retired_slots_tracked(inner: &LioInner) {
    let tracked = inner.dying.len() + inner.burying + inner.leaked;
    assert_eq!(
      inner.store.retired(),
      tracked,
      "driver: {} slots are retired, but {tracked} registrations are waiting \
       to be dropped, so a slot is never released",
      inner.store.retired()
    );
  }

  /// Whether a new operation would have to wait for a free slot.
  ///
  /// [`Lio::new`] reserves capacity for `cap` operations in flight at once.
  /// Futures over capacity wait for a slot; [`Io::when_done`],
  /// [`Io::send`] and [`Io::send_with`] queue the operation instead, which
  /// allocates. Callers that want strict, allocation-free backpressure can
  /// check this before submitting.
  ///
  /// [`Io::when_done`]: crate::api::Io::when_done
  /// [`Io::send`]: crate::api::Io::send
  /// [`Io::send_with`]: crate::api::Io::send_with
  pub fn is_full(&self) -> bool {
    let inner = self.inner.borrow();
    !inner.overflow.is_empty() || inner.store.is_full()
  }

  /// Wakes `waker` once a slot frees up.
  pub(crate) fn wait_for_capacity(&self, waker: &Waker) {
    let mut inner = self.inner.borrow_mut();
    // Only freeing a slot wakes the waiters, so with one free already
    // nothing ever would.
    assert!(
      !inner.overflow.is_empty() || inner.store.is_full(),
      "driver: waiting for capacity while a slot is free"
    );
    if !inner.capacity_waiters.iter().any(|w| w.will_wake(waker)) {
      inner.capacity_waiters.push(waker.clone());
    }
  }

  /// Hands freed slots to queued submissions, then (if slots remain) returns
  /// the futures waiting for capacity, to be woken without the borrow.
  fn release_capacity(inner: &mut LioInner) -> Vec<Waker> {
    if inner.overflow.is_empty() && inner.capacity_waiters.is_empty() {
      return Vec::new();
    }
    while !inner.store.is_full()
      && let Some(queued) = inner.overflow.pop_front()
    {
      if queued.token.as_ref().is_some_and(|token| token.abandoned.get()) {
        inner.abandoned.push(queued);
        continue;
      }
      let Queued { init, token, .. } = queued;
      let at = init(inner, None);
      if let Some(token) = token {
        token.at.set(Some(at));
      }
    }
    if inner.store.is_full() {
      Vec::new()
    } else {
      std::mem::take(&mut inner.capacity_waiters)
    }
  }

  /// Non-blocking poll for completed operations.
  ///
  /// Returns immediately, processing any completions that are ready.
  #[inline]
  pub fn try_run(&self) -> io::Result<usize> {
    self.run_inner(Some(Duration::ZERO))
  }

  /// Block until at least one operation completes.
  #[inline]
  pub fn run(&self) -> io::Result<usize> {
    self.run_inner(None)
  }

  /// Run the event loop with a timeout.
  ///
  /// Waits for at least one operation to complete or until the timeout expires,
  /// whichever comes first. Returns `Ok(true)` if completions were processed,
  /// `Ok(false)` if the timeout expired with no completions.
  #[inline]
  pub fn run_timeout(&self, timeout: Duration) -> io::Result<usize> {
    self.run_inner(Some(timeout))
  }

  fn run_inner(&self, timeout: Option<Duration>) -> io::Result<usize> {
    let inner = self.inner.borrow_mut();
    let ran = if inner.profile.is_some() {
      self.run_with::<true>(inner, timeout)
    } else {
      self.run_with::<false>(inner, timeout)
    };
    // After an error, what this run removed is still dropped. After a run
    // that left nothing behind, there is nothing to do.
    if ran.as_ref().map_or(true, |ran| ran.2) {
      self.bury_and_wake();
      self.poll_woken();
    }
    let (processed, waiters, _) = ran?;
    for waker in waiters {
      waker.wake();
    }
    Ok(processed)
  }

  /// Drops the registrations removed while the driver was borrowed, then
  /// hands the slots that frees to queued submissions and wakes the futures
  /// waiting for capacity.
  ///
  /// Dropping a registration runs user code (its op model, callback and
  /// buffers), which may use this `Lio`, so it happens with the driver not
  /// borrowed. Each slot is released only after its registration is gone: the
  /// registration points into the slot's arenas, which reuse would reset.
  ///
  /// One borrow drains every registration waiting in `dying`. A single one is
  /// dropped and its slot released before anything else is woken. Several are
  /// dropped first, then their slots are released together.
  fn bury_and_wake(&self) {
    loop {
      // Queued submissions nobody waits for any more hold user values:
      // dropped here, with the driver not borrowed.
      let abandoned = std::mem::take(&mut self.inner.borrow_mut().abandoned);
      drop(abandoned);
      let mut inner = self.inner.borrow_mut();
      if inner.dying.is_empty() {
        // Slots given back since the last time may be wanted.
        let waiters = Self::release_capacity(&mut inner);
        drop(inner);
        for waker in waiters {
          waker.wake();
        }
        return;
      }
      if inner.dying.len() == 1 {
        let Some(Detached { registration, slot }) = inner.dying.pop() else {
          unreachable!(
            "driver: dying reported one registration and did not have one"
          );
        };
        inner.burying += 1;
        drop(inner);
        {
          // Released even if the drop panics.
          let _release = ReleaseSlot { lio: self, slot };
          drop(registration);
        }
      } else {
        // One borrow drains every registration removed so far. Drops happen
        // below, with the driver not borrowed. Slots are released after
        // those drops, in one borrow, unless a drop panics.
        let pending = std::mem::take(&mut inner.dying);
        let slots = std::mem::take(&mut inner.released_slots);
        inner.burying += pending.len();
        drop(inner);
        let mut batch = BuryBatch { lio: self, pending, slots, dropping: None };
        // Up to 256 slots, one release borrow after every drop is cheaper
        // than borrowing per registration. Past that, a second pass walks
        // slots that have left the cache, so release every 64 while the
        // registrations just dropped are still hot.
        let release_every =
          if batch.pending.len() > 256 { 64 } else { usize::MAX };
        while let Some(Detached { registration, slot }) = batch.pending.pop() {
          batch.dropping = Some(slot);
          drop(registration);
          batch.dropping = None;
          batch.slots.push(slot);
          if batch.slots.len() == release_every {
            self.release_buried_slots(&mut batch.slots);
          }
        }
        self.release_buried_slots(&mut batch.slots);
        let waiters = {
          let mut inner = self.inner.borrow_mut();
          // Hand the buffers back. A nested bury may already have retired
          // something into the fresh `dying` queue; keep those.
          let spare_dying = std::mem::take(&mut batch.pending);
          if inner.dying.is_empty() {
            inner.dying = spare_dying;
          }
          inner.released_slots = std::mem::take(&mut batch.slots);
          Self::release_capacity(&mut inner)
        };
        // Slots are released and the buffers are back in the driver.
        // Skipping `Drop` avoids that out-of-line call on the path that
        // already finished.
        std::mem::forget(batch);
        for waker in waiters {
          waker.wake();
        }
        continue;
      }

      // Scheduling queued submissions may remove ops without a first action,
      // so go around again until nothing is left to bury.
      let waiters = {
        let mut inner = self.inner.borrow_mut();
        Self::release_capacity(&mut inner)
      };
      for waker in waiters {
        waker.wake();
      }
    }
  }

  /// Releases slots whose registrations were just dropped. One borrow covers
  /// the whole chunk. `burying` already counts these slots.
  #[inline]
  fn release_buried_slots(&self, slots: &mut Vec<u32>) {
    if slots.is_empty() {
      return;
    }
    let mut inner = self.inner.borrow_mut();
    for slot in slots.drain(..) {
      inner.store.release(slot);
      inner.burying -= 1;
    }
    Lio::assert_retired_slots_tracked(&inner);
  }

  /// One driver turn.
  ///
  /// Submitting and waiting run under the borrow. Routing completions holds
  /// it only for bookkeeping: each handler runs with it released, because
  /// handlers run user code (callbacks, and wakers that may poll inline)
  /// which may schedule, drop or drive operations on this `Lio`.
  fn run_with<'a, const PROFILE: bool>(
    &'a self,
    mut inner: RefMut<'a, LioInner>,
    timeout: Option<Duration>,
  ) -> io::Result<(usize, Vec<Waker>, bool)> {
    let mut stats = LioProfile::default();
    let inner_ref = &mut *inner;

    timed!(PROFILE, stats, flush_time, inner_ref.io.flush())?;

    // Work already queued is ready now; don't block on the backend first.
    let queued = !inner_ref.ready_io.is_empty()
      || !inner_ref.ready_timers.is_empty()
      || !inner_ref.woken.is_empty();
    let effective_timeout = if queued {
      Some(Duration::ZERO)
    } else {
      timed!(
        PROFILE,
        stats,
        next_deadline_time,
        inner_ref.time.wait_timeout(timeout)
      )
    };

    inner_ref.completed.clear();
    let (io, completed) = (&mut inner_ref.io, &mut inner_ref.completed);
    let waited =
      timed!(PROFILE, stats, wait_time, io.wait(effective_timeout, completed));
    // Queue what the backend reported even if it then failed, so those
    // operations are not lost.
    inner_ref.ready_io.extend(inner_ref.completed.drain(..));
    waited?;

    let before = inner_ref.ready_timers.len();
    let ready_timers = &mut inner_ref.ready_timers;
    // `for_each` takes the expiry iterator's `fold`, which skips the
    // per-timer staleness check when no fired timer went stale; `extend`
    // would step it with `next`.
    timed!(
      PROFILE,
      stats,
      timer_poll_time,
      inner_ref.time.poll_expired().for_each(|id| ready_timers.push_back(id))
    );
    stats.count_timer_expirations(before, inner_ref.ready_timers.len());
    let resource_close = inner_ref.resource_close;

    // Route what is queued now, not what nested runs queue meanwhile, so
    // handlers that keep producing work cannot keep this run going forever.
    let mut budget = inner_ref.ready_io.len() + inner_ref.ready_timers.len();
    let loop_started = PROFILE.then(Instant::now);
    let mut batch = Batch { lio: self, current: None };
    let mut processed = 0;
    while budget > 0 {
      budget -= 1;
      let (id, completion, timer) = if let Some(c) = inner.ready_io.pop_front()
      {
        let completion =
          Completion::new(c.result()).with_resource_close(resource_close);
        (c.registration_id(), completion, false)
      } else if let Some(id) = inner.ready_timers.pop_front() {
        (id, TimeManager::fired_completion(), true)
      } else {
        // A nested run routed the rest.
        break;
      };
      let routed;
      (inner, routed) = self
        .route::<PROFILE>(inner, &mut batch, id, completion, timer, &mut stats);
      if routed {
        processed += 1;
      } else {
        stats.count_stale_completion();
      }
    }
    stats.end_completion_loop(loop_started);

    if PROFILE && let Some(profile) = inner.profile.as_mut() {
      stats.completions_returned = processed;
      profile.merge(&stats);
    }
    let waiters = Self::release_capacity(&mut inner);
    // Whether this run left something to drop or a model to poll, which
    // happens with the driver not borrowed.
    let unfinished = !inner.dying.is_empty()
      || !inner.abandoned.is_empty()
      || !inner.woken.is_empty();
    Ok((processed, waiters, unfinished))
  }

  /// Routes one completion to its registration, releasing `inner` only while
  /// the registration's handler runs. Returns the reacquired borrow and
  /// whether the registration still existed.
  #[inline(always)]
  fn route<'a, const PROFILE: bool>(
    &'a self,
    mut inner: RefMut<'a, LioInner>,
    batch: &mut Batch<'_>,
    id: u64,
    completion: Completion,
    timer: bool,
    stats: &mut LioProfile,
  ) -> (RefMut<'a, LioInner>, bool) {
    if timer {
      inner.time.remove(id);
    }
    let begun = timed!(
      PROFILE,
      stats,
      completion_store_lookup_time,
      inner.store.get_mut(id).and_then(|reg| reg.begin_completion())
    );
    let Some(raw) = begun else {
      // An earlier handler may have cancelled a sleep whose timer already
      // fired. An I/O operation, though, stays in its slot until its
      // completion arrives: one without it was completed twice, or never
      // pushed.
      assert!(
        timer,
        "driver: backend completed operation {id}, which is not in flight"
      );
      return (inner, false);
    };

    drop(inner);
    batch.current = Some(id);
    timed!(PROFILE, stats, completion_on_completion_time, {
      // SAFETY: the registration was alive above and its operation was just
      // marked busy, which keeps the slot from being given back. An
      // abandoned operation's result is still read, so what it produced (an
      // accepted socket, say) is released.
      unsafe { raw.complete(completion) }
    });
    batch.current = None;
    let mut inner = self.inner.borrow_mut();

    let inner_ref = &mut *inner;
    let Some(reg) = inner_ref.store.get_mut(id) else {
      panic!("driver: slot {id} was given back while its result was read");
    };
    match reg.end_completion(raw.kept()) {
      // The model that was waiting for it takes it now. A model that is
      // busy further up the stack is polled again when it is not.
      Some(waiter) => {
        if let Some(begun) = Self::begin(inner_ref, waiter) {
          // A slot this empties is given back when the run is over.
          (inner, _) = self.poll_begun(inner, waiter, begun);
        }
      }
      None if reg.is_spent() => {
        // Dropped by `bury_and_wake` once the driver is not borrowed.
        timed!(PROFILE, stats, remove_time, Self::retire(inner_ref, id));
        stats.count_remove();
      }
      // An abandoned operation of a model that is still running.
      None => Self::start_waiting_at_home(inner_ref, id),
    }
    (inner, true)
  }
}

/// Tracks the registration whose handler is running during a run.
///
/// If the handler panics, dropping the batch removes that registration: its
/// state is unknown. It is dropped on the next run rather than during the
/// unwind, where a second panic would abort. The entries the run did not
/// reach stay queued in `LioInner`, so their operations still complete on the
/// next run.
struct Batch<'a> {
  lio: &'a Lio,
  /// The registration whose handler is running.
  current: Option<u64>,
}

impl Drop for Batch<'_> {
  fn drop(&mut self) {
    let Some(id) = self.current.take() else {
      return;
    };
    if let Ok(mut inner) = self.lio.inner.try_borrow_mut()
      && let Some(reg) = inner.store.get_mut(id)
    {
      reg.completion_panicked();
      if reg.is_spent()
        && let Some(detached) = inner.store.detach(id)
      {
        inner.dying.push(detached);
      }
    }
  }
}

/// Marks a model as broken if polling it unwinds: it is dropped by the next
/// run, not during the unwind, where a second panic would abort.
struct PollGuard<'a> {
  lio: &'a Lio,
  id: u64,
  armed: bool,
}

impl Drop for PollGuard<'_> {
  fn drop(&mut self) {
    if !self.armed {
      return;
    }
    if let Ok(mut inner) = self.lio.inner.try_borrow_mut() {
      let inner = &mut *inner;
      if let Some(reg) = inner.store.get_mut(self.id) {
        reg.poll_panicked();
        Lio::wake(inner, self.id);
      }
    }
  }
}

/// Frees an operation's memory if building its action unwinds, before the
/// backend has seen it.
struct AbortOp<'a> {
  reg: &'a mut Registration,
  armed: bool,
}

impl Drop for AbortOp<'_> {
  fn drop(&mut self) {
    if self.armed {
      self.reg.abort_op();
    }
  }
}

/// Registrations drained from `dying` in one borrow.
///
/// Their drops run with the driver not borrowed. Slots already dropped are
/// released together afterwards. A panic during a drop releases the slot
/// being dropped and the ones already dropped, and puts the rest back on
/// `dying` so a later bury drops them.
struct BuryBatch<'a> {
  lio: &'a Lio,
  pending: Vec<Detached>,
  slots: Vec<u32>,
  dropping: Option<u32>,
}

impl Drop for BuryBatch<'_> {
  fn drop(&mut self) {
    let Ok(mut inner) = self.lio.inner.try_borrow_mut() else {
      // The slots leak. Only a panic, already reporting a bug, can leave
      // the driver borrowed here; panicking again would abort.
      assert!(
        std::thread::panicking(),
        "driver: {} buried registrations are never dropped: the driver is borrowed",
        self.pending.len()
      );
      return;
    };
    if let Some(slot) = self.dropping.take() {
      inner.store.release(slot);
      inner.burying -= 1;
    }
    for slot in self.slots.drain(..) {
      inner.store.release(slot);
      inner.burying -= 1;
    }
    let n = self.pending.len();
    inner.burying -= n;
    if inner.dying.is_empty() {
      inner.dying = std::mem::take(&mut self.pending);
    } else {
      inner.dying.append(&mut self.pending);
    }
    Lio::assert_retired_slots_tracked(&inner);
  }
}

/// Releases a buried registration's slot, also if dropping it panicked.
struct ReleaseSlot<'a> {
  lio: &'a Lio,
  slot: u32,
}

impl Drop for ReleaseSlot<'_> {
  fn drop(&mut self) {
    let panicking = std::thread::panicking();
    let Ok(mut inner) = self.lio.inner.try_borrow_mut() else {
      // The slot leaks. Only a panic, already reporting a bug, can leave
      // the driver borrowed here; panicking again would abort.
      assert!(
        panicking,
        "driver: slot {} is never released: the driver is borrowed",
        self.slot
      );
      return;
    };
    inner.store.release(self.slot);
    inner.burying -= 1;
    if !panicking {
      Lio::assert_retired_slots_tracked(&inner);
    }
  }
}

#[cfg(test)]
#[path = "lio_model_tests.rs"]
mod model_tests;

#[cfg(test)]
#[path = "lio_lifecycle_tests.rs"]
mod lifecycle_tests;

#[cfg(test)]
mod tests {
  use super::*;
  use crate::backend::testing::ScriptedBackend;

  fn lio(cap: usize) -> (ScriptedBackend, Lio) {
    let backend = ScriptedBackend::new();
    (backend.clone(), Lio::new_with_backend(backend, cap).unwrap())
  }

  #[test]
  #[should_panic(expected = "with its timer still scheduled")]
  fn removing_a_registration_with_a_live_timer_panics() {
    let (backend, lio) = lio(1);
    crate::api::nop().with_lio(&lio).when_done(|_| {});
    let id = backend.pushed()[0];
    let mut inner = lio.inner.borrow_mut();
    inner.time.schedule(id, Duration::from_secs(60));
    Lio::retire(&mut inner, id);
  }

  #[test]
  #[should_panic(expected = "so a slot is never released")]
  fn a_slot_detached_without_release_panics() {
    let (backend, lio) = lio(2);
    crate::api::nop().with_lio(&lio).when_done(|_| {});
    crate::api::nop().with_lio(&lio).when_done(|_| {});
    let (lost, other) = (backend.pushed()[0], backend.pushed()[1]);
    // `lost` is detached and dropped with no slot release. Dropped with the
    // driver not borrowed: it holds a model.
    let detached = lio.inner.borrow_mut().store.detach(lost).unwrap();
    drop(detached.registration);
    let detached = lio.inner.borrow_mut().store.detach(other).unwrap();
    let mut inner = lio.inner.borrow_mut();
    inner.dying.push(detached);
    Lio::assert_retired_slots_tracked(&inner);
  }

  #[test]
  #[should_panic(expected = "so a slot is never released")]
  fn releasing_a_slot_checks_every_retired_slot_is_tracked() {
    let (backend, lio) = lio(2);
    crate::api::nop().with_lio(&lio).when_done(|_| {});
    crate::api::nop().with_lio(&lio).when_done(|_| {});
    let (lost, other) = (backend.pushed()[0], backend.pushed()[1]);
    let detached = lio.inner.borrow_mut().store.detach(lost).unwrap();
    drop(detached.registration);
    // Queued for release without the check `retire` makes.
    let detached = lio.inner.borrow_mut().store.detach(other).unwrap();
    lio.inner.borrow_mut().dying.push(detached);
    lio.bury_and_wake();
  }

  #[test]
  fn retiring_while_a_registration_is_dropped_keeps_slots_accounted() {
    use std::future::{Future, IntoFuture};
    use std::task::Context;

    let (backend, lio) = lio(2);
    let mut sleep = Box::pin(
      crate::api::sleep(Duration::from_secs(60)).with_lio(&lio).into_future(),
    );
    let mut cx = Context::from_waker(Waker::noop());
    assert!(sleep.as_mut().poll(&mut cx).is_pending());
    // Dropping the callback drops the sleep, whose cancellation retires its
    // registration while the callback's slot is still being released.
    crate::api::nop().with_lio(&lio).when_done(move |_| {
      let _ = &sleep;
    });
    backend.complete_all(0);
    lio.try_run().unwrap();
    assert!(!lio.is_full(), "both slots are free again");
  }

  #[test]
  #[cfg_attr(miri, ignore = "leaks the registration on purpose")]
  fn a_held_lease_keeps_the_store_after_the_driver_drops() {
    let (backend, lio) = lio(1);
    crate::api::nop().with_lio(&lio).when_done(|_| {});
    let id = backend.pushed()[0];
    {
      let mut inner = lio.inner.borrow_mut();
      Lio::retire(&mut inner, id);
      assert_eq!(inner.leaked, 1, "a held lease was not counted");
    }
    STORE_DROPPED.with(|dropped| dropped.set(false));
    drop(lio);
    assert!(
      !STORE_DROPPED.with(|dropped| dropped.get()),
      "the store was freed while a registration was leaked"
    );
  }

  #[test]
  fn completing_one_op_drops_only_its_lease() {
    let (backend, lio) = lio(2);
    crate::api::nop().with_lio(&lio).when_done(|_| {});
    crate::api::nop().with_lio(&lio).when_done(|_| {});
    let first = backend.in_flight()[0];
    let second = backend.in_flight()[1];
    backend.complete(first, 0);
    lio.try_run().unwrap();
    assert_eq!(lio.inner.borrow().leaked, 0);
    assert_eq!(backend.in_flight(), vec![second]);
  }

  #[test]
  fn dropping_a_drained_driver_frees_the_store() {
    let (backend, lio) = lio(1);
    crate::api::nop().with_lio(&lio).when_done(|_| {});
    let id = backend.pushed()[0];
    backend.complete(id, 0);
    lio.try_run().unwrap();
    STORE_DROPPED.with(|dropped| dropped.set(false));
    drop(lio);
    assert!(STORE_DROPPED.with(|dropped| dropped.get()));
  }

  /// Reports a completion for every pushed op, twice.
  #[derive(Default)]
  struct Twice(Vec<u64>);

  // SAFETY: test backend. It never dereferences an address. It completes
  // twice on purpose.
  unsafe impl IoBackend for Twice {
    fn init(&mut self, _: usize) -> io::Result<()> {
      Ok(())
    }
    fn push(&mut self, id: u64, _: crate::backend::InFlight<'_>, _: &mut Bump) {
      self.0.extend([id, id]);
    }
    fn flush(&mut self) -> io::Result<()> {
      Ok(())
    }
    fn wait(
      &mut self,
      _: Option<Duration>,
      completed: &mut Vec<OpCompleted>,
    ) -> io::Result<()> {
      completed.clear();
      completed.extend(self.0.drain(..).map(|id| OpCompleted::new(id, 0)));
      Ok(())
    }
  }

  #[test]
  #[should_panic(expected = "which is not in flight")]
  fn a_second_completion_for_an_operation_panics() {
    let lio = Lio::new_with_backend(Twice::default(), 1).unwrap();
    crate::api::nop().with_lio(&lio).when_done(|_| {});
    lio.try_run().unwrap();
  }

  #[test]
  fn a_timer_firing_after_its_sleep_was_cancelled_is_ignored() {
    use std::cell::RefCell;
    use std::future::{Future, IntoFuture};
    use std::rc::Rc;
    use std::task::Context;

    let (_, lio) = lio(2);
    crate::time::pause(&lio);
    let later = Rc::new(RefCell::new(Some(Box::pin(
      crate::api::sleep(Duration::from_millis(2)).with_lio(&lio).into_future(),
    ))));
    let mut cx = Context::from_waker(Waker::noop());
    assert!(
      later.borrow_mut().as_mut().unwrap().as_mut().poll(&mut cx).is_pending()
    );
    // Fires first, in the same run, and drops the later sleep.
    let first = later.clone();
    crate::api::sleep(Duration::from_millis(1))
      .with_lio(&lio)
      .when_done(move |_| drop(first.borrow_mut().take()));
    lio.advance_time_by_ticks(5);
    lio.try_run().unwrap();
    assert!(later.borrow().is_none());
    assert!(!lio.is_full());
  }

  #[test]
  #[should_panic(expected = "waiting for capacity while a slot is free")]
  fn waiting_for_capacity_with_a_free_slot_panics() {
    let (_, lio) = lio(1);
    lio.wait_for_capacity(Waker::noop());
  }

  /// Records being dropped.
  struct DropFlag(Rc<Cell<bool>>);

  impl Drop for DropFlag {
    fn drop(&mut self) {
      self.0.set(true);
    }
  }

  impl crate::model::Cx<'_> {
    /// An operation that panics when the driver asks what to submit.
    fn panicking(&mut self, memory: DropFlag) -> crate::model::Ticket<()> {
      self.submit(memory, |_| panic!("no action for you"), |_, _| ())
    }
  }

  leaf!([] struct Panicking(memory: DropFlag) -> () = cx.panicking);

  #[test]
  fn a_panic_while_building_the_action_does_not_keep_its_slot() {
    // Capacity 1: if the panicking op kept its slot, nothing more would run.
    let (backend, lio) = lio(1);
    let dropped = Rc::new(Cell::new(false));
    let op = Panicking::new(DropFlag(dropped.clone()));

    let submit = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
      crate::api::Io::from_op(op).with_lio(&lio).when_done(|_| {});
    }));
    assert!(submit.is_err(), "the panic reaches the submitter");
    assert!(dropped.get(), "the op's memory was released");
    // The model is dropped by the next run, not during the unwind.
    lio.try_run().unwrap();
    assert!(!lio.is_full(), "and its slot with it");

    let done = Rc::new(Cell::new(false));
    let set = done.clone();
    crate::api::nop().with_lio(&lio).when_done(move |_| set.set(true));
    backend.complete_all(0);
    lio.try_run().unwrap();
    assert!(done.get());
  }

  #[test]
  fn a_model_started_on_a_full_driver_waits_for_a_slot() {
    let (backend, lio) = lio(1);
    crate::api::nop().with_lio(&lio).when_done(|_| {});
    let ran = Rc::new(Cell::new(false));
    let set = ran.clone();
    let queued = lio.spawn(crate::api::nop().into_inner(), move |_| {
      set.set(true);
      true
    });
    assert!(matches!(queued, Spawned::Queued(_)));
    assert_eq!(backend.pushed().len(), 1, "only the first has a slot");

    backend.complete_all(0);
    lio.try_run().unwrap();
    assert_eq!(backend.pushed().len(), 2, "the second took the freed slot");
    backend.complete_all(0);
    lio.try_run().unwrap();
    assert!(ran.get());
    assert!(!lio.is_full());
  }

  #[test]
  fn a_model_cancelled_while_it_waits_for_a_slot_never_starts() {
    let (backend, lio) = lio(1);
    crate::api::nop().with_lio(&lio).when_done(|_| {});
    let queued = lio.spawn(crate::api::nop().into_inner(), |_| true);
    lio.cancel(&queued);

    backend.complete_all(0);
    lio.try_run().unwrap();
    lio.try_run().unwrap();
    assert_eq!(backend.pushed().len(), 1, "the cancelled model never ran");
    assert!(!lio.is_full());
  }

  /// Records where each operation's first lowering allocation lands.
  #[derive(Clone, Default)]
  struct Lowering {
    inner: ScriptedBackend,
    first_allocs: std::rc::Rc<std::cell::RefCell<Vec<usize>>>,
  }

  // SAFETY: every call goes to the inner `ScriptedBackend`.
  unsafe impl IoBackend for Lowering {
    fn init(&mut self, cap: usize) -> io::Result<()> {
      self.inner.init(cap)
    }
    fn push(
      &mut self,
      id: u64,
      op: crate::backend::InFlight<'_>,
      bump: &mut Bump,
    ) {
      let at = bump.alloc(0u64) as *mut u64 as usize;
      self.first_allocs.borrow_mut().push(at);
      self.inner.push(id, op, bump);
    }
    fn flush(&mut self) -> io::Result<()> {
      self.inner.flush()
    }
    fn drain(&mut self) -> bool {
      self.inner.drain()
    }
    fn wait(
      &mut self,
      timeout: Option<Duration>,
      completed: &mut Vec<OpCompleted>,
    ) -> io::Result<()> {
      self.inner.wait(timeout, completed)
    }
  }

  #[test]
  fn every_operation_lowers_into_its_slot_s_reset_arena() {
    let backend = Lowering::default();
    let lio = Lio::new_with_backend(backend.clone(), 1).unwrap();
    for _ in 0..2 {
      crate::api::nop().with_lio(&lio).when_done(|_| {});
      backend.inner.complete_all(0);
      lio.try_run().unwrap();
    }
    let allocs = backend.first_allocs.borrow().clone();
    assert_eq!(allocs.len(), 2);
    assert_eq!(
      allocs[0], allocs[1],
      "the second operation reused the slot's arena"
    );
  }

  #[test]
  fn a_run_routes_only_what_was_queued_when_it_started() {
    use std::cell::RefCell;
    use std::rc::Rc;

    let (backend, lio) = lio(3);
    let delivered = Rc::new(RefCell::new(Vec::new()));
    // A's handler completes B and C, then runs the driver itself; B's
    // handler panics there, leaving C queued.
    let (nested, inner_backend) = (lio.clone(), backend.clone());
    crate::api::nop().with_lio(&lio).when_done(move |_| {
      inner_backend.complete_all(0);
      let run = std::panic::AssertUnwindSafe(|| nested.try_run());
      assert!(std::panic::catch_unwind(run).is_err());
    });
    crate::api::nop().with_lio(&lio).when_done(|_| panic!("handler B"));
    let log = delivered.clone();
    crate::api::nop()
      .with_lio(&lio)
      .when_done(move |_| log.borrow_mut().push('C'));

    let a = backend.in_flight()[0];
    backend.complete(a, 0);
    assert_eq!(lio.try_run().unwrap(), 1, "only A was queued");
    assert!(delivered.borrow().is_empty(), "C waits for the next run");
    assert_eq!(lio.try_run().unwrap(), 1);
    assert_eq!(*delivered.borrow(), ['C']);
    assert!(!lio.is_full(), "B's slot was given back too");
  }

  #[test]
  #[should_panic(expected = "queued a submission while a slot is free")]
  fn queueing_with_a_free_slot_panics() {
    let (_, lio) = lio(1);
    Lio::enqueue(
      &mut lio.inner.borrow_mut(),
      |_, _| unreachable!(),
      None,
      None,
    );
  }

  #[test]
  fn pause_and_resume_stop_and_restart_the_drivers_clock() {
    let (_, lio) = lio(1);
    crate::time::pause(&lio);
    assert!(lio.inner.borrow().time.is_paused());
    crate::time::resume(&lio);
    assert!(!lio.inner.borrow().time.is_paused());
  }

  #[test]
  fn uninstalling_the_global_returns_it() {
    let (_, lio) = lio(1);
    let guard = crate::install_global(lio.clone());
    let global = crate::uninstall_global().expect("it was installed");
    assert!(Rc::ptr_eq(&global.inner, &lio.inner));
    assert!(crate::uninstall_global().is_none(), "and is gone");
    drop(guard);
  }

  #[test]
  fn run_reports_how_many_completions_it_routed() {
    let lio = Lio::new_with_backend(ScriptedBackend::auto(0), 4).unwrap();
    for _ in 0..2 {
      crate::api::nop().with_lio(&lio).when_done(|_| {});
    }
    assert_eq!(lio.run().unwrap(), 2);
  }

  /// Completes every op on flush, and records each wait's timeout.
  struct Recording {
    inner: ScriptedBackend,
    timeouts: Rc<RefCell<Vec<Option<Duration>>>>,
  }

  // SAFETY: every call goes to the inner `ScriptedBackend`.
  unsafe impl IoBackend for Recording {
    fn init(&mut self, cap: usize) -> io::Result<()> {
      self.inner.init(cap)
    }
    fn resource_close(&self) -> crate::api::resource::ResourceClose {
      self.inner.resource_close()
    }
    fn push(
      &mut self,
      id: u64,
      op: crate::backend::InFlight<'_>,
      bump: &mut Bump,
    ) {
      self.inner.push(id, op, bump);
    }
    fn flush(&mut self) -> io::Result<()> {
      self.inner.flush()
    }
    fn wait(
      &mut self,
      timeout: Option<Duration>,
      completed: &mut Vec<OpCompleted>,
    ) -> io::Result<()> {
      self.timeouts.borrow_mut().push(timeout);
      self.inner.wait(timeout, completed)
    }
  }

  #[test]
  fn queued_completions_do_not_wait_on_the_backend() {
    let timeouts = Rc::new(RefCell::new(Vec::new()));
    let backend =
      Recording { inner: ScriptedBackend::auto(0), timeouts: timeouts.clone() };
    let lio = Lio::new_with_backend(backend, 4).unwrap();
    crate::api::nop().with_lio(&lio).when_done(|_| panic!("handler failure"));
    crate::api::nop().with_lio(&lio).when_done(|_| {});
    let first = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
      lio.try_run().unwrap();
    }));
    assert!(first.is_err());

    // The second completion is still queued, so even a blocking run must
    // not block on the backend to route it.
    lio.run().unwrap();
    assert_eq!(timeouts.borrow().last(), Some(&Some(Duration::ZERO)));
  }

  #[test]
  fn a_panicking_handler_still_frees_its_slot() {
    let lio = Lio::new_with_backend(ScriptedBackend::auto(0), 1).unwrap();
    crate::api::nop().with_lio(&lio).when_done(|_| panic!("handler failure"));
    let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
      lio.try_run().unwrap();
    }));
    assert!(run.is_err());
    lio.try_run().unwrap();
    assert!(!lio.is_full(), "the panicking op's slot is released");
  }

  /// Records whether the driver was full when its registration was dropped.
  struct NoteFull {
    lio: Lio,
    full: Rc<RefCell<Vec<bool>>>,
  }

  impl Drop for NoteFull {
    fn drop(&mut self) {
      self.full.borrow_mut().push(self.lio.is_full());
    }
  }

  /// Submits `cap` nops whose drops record `is_full`, then buries them all.
  fn full_while_burying(cap: usize) -> Vec<bool> {
    let lio = Lio::new_with_backend(ScriptedBackend::auto(0), cap).unwrap();
    let full = Rc::new(RefCell::new(Vec::with_capacity(cap)));
    for _ in 0..cap {
      let note = NoteFull { lio: lio.clone(), full: Rc::clone(&full) };
      crate::api::nop().with_lio(&lio).when_done(move |_| {
        let _note = &note;
      });
    }
    lio.try_run().unwrap();
    full.take()
  }

  #[test]
  fn a_slot_goes_back_as_soon_as_its_model_is_done() {
    // What a model and its consumer hold is dropped when the model
    // finishes, with its own slot still taken.
    assert_eq!(full_while_burying(1), [true]);
    // The slots of the models that finished before it are free by then.
    let full = full_while_burying(257);
    assert_eq!(full.len(), 257);
    assert!(full[0], "nothing has finished before the first");
    assert!(full[1..].iter().all(|full| !*full));

    let lio = Lio::new_with_backend(ScriptedBackend::auto(0), 257).unwrap();
    for _ in 0..257 {
      crate::api::nop().with_lio(&lio).when_done(|_| {});
    }
    assert!(lio.is_full());
    lio.try_run().unwrap();
    assert!(!lio.is_full());
    assert_eq!(lio.inner.borrow().store.retired(), 0);
  }

  /// Panics when the consumer that captured it is dropped.
  struct Bomb;

  impl Drop for Bomb {
    fn drop(&mut self) {
      panic!("buried registration");
    }
  }

  #[test]
  fn a_panic_while_burying_a_batch_releases_the_slots_already_dropped() {
    let lio = Lio::new_with_backend(ScriptedBackend::auto(0), 3).unwrap();
    crate::api::nop().with_lio(&lio).when_done(|_| {});
    let bomb = Bomb;
    crate::api::nop().with_lio(&lio).when_done(move |_| {
      let _bomb = &bomb;
    });
    crate::api::nop().with_lio(&lio).when_done(|_| {});
    let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
      lio.try_run().unwrap();
    }));
    assert!(run.is_err(), "the buried registration panicked");
    // The one not yet dropped is still on `dying`; this bury drops it.
    lio.try_run().unwrap();
    assert!(!lio.is_full(), "every slot of the batch is released");
    crate::api::nop().with_lio(&lio).when_done(|_| {});
    lio.try_run().unwrap();
    assert!(!lio.is_full(), "a later registration still accounts for its slot");
  }

  #[test]
  fn completing_an_op_on_its_slot_s_last_generation_keeps_the_capacity() {
    let backend = ScriptedBackend::auto(0);
    let lio = Lio::new_with_backend(backend.clone(), 1).unwrap();
    lio.inner.borrow_mut().store.set_generation(0, u32::MAX);
    let done = Rc::new(RefCell::new(0));
    for _ in 0..2 {
      let done = done.clone();
      crate::api::nop()
        .with_lio(&lio)
        .when_done(move |_| *done.borrow_mut() += 1);
      assert_eq!(lio.try_run().unwrap(), 1);
      assert!(!lio.is_full(), "the slot, or its replacement, is free");
    }
    assert_eq!(*done.borrow(), 2);
    let [spent, fresh] = backend.pushed()[..] else { panic!("two ops") };
    assert_eq!(spent, u64::from(u32::MAX) << 32);
    assert_eq!(fresh, 1, "the replacement slot, at its first generation");
    assert!(lio.inner.borrow_mut().store.get_mut(spent).is_none());
  }
}
