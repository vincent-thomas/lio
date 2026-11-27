use crate::{
  api::op::{Action, Completion},
  backend::{
    IoBackend, OpCompleted, Tracked,
    store::{Detached, OpStore},
  },
  model::{Cx, Drive, Turn},
  registration::{Abandon, AfterPoll, Mail, RawTask, Registration},
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
  // The backend, and the table of what it was given, which holds the step
  // arenas. Dropped by hand in `Drop`: the backend must have reported what
  // was in flight before `store` frees the memory that work points into,
  // and both are leaked when it has not.
  io: ManuallyDrop<Tracked<Box<dyn IoBackend>>>,
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
  /// How many more operations the model being polled may have finished
  /// as they are started. See [`Lio::finish_started`].
  started_left: u32,
  /// How many models are being polled, one inside another.
  polling: u32,
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
  /// Done already: it never needed a slot.
  Done,
  /// In this slot.
  Now(u64),
  /// Waiting for a slot.
  Queued(Rc<QueueToken>),
}

impl Spawned {
  /// The slot it is in, once it has one.
  fn id(&self) -> Option<u64> {
    match self {
      Spawned::Done => None,
      Spawned::Now(id) => Some(*id),
      Spawned::Queued(token) => token.id(),
    }
  }
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
  /// The backend performed it as it was offered: it never had a slot.
  Finished,
  /// Started by a model that had no slot yet, with every slot taken: it is
  /// started once that model has one.
  Unplaced(Option<BoxedPlace>),
  /// Operation `seq` of slot `id`.
  At { id: u64, seq: u64 },
  /// Waiting for a slot of its own, where it will be the first operation.
  Queued(Rc<QueueToken>),
}

/// [`PlaceOp`], kept for later.
pub(crate) type BoxedPlace =
  Box<dyn FnOnce(&mut Registration, &mut Bump, u64) -> u64>;

/// How a model's first poll, made before it has a slot, ended.
pub(crate) enum Trial<M> {
  /// The model is done, and dropped.
  Finished,
  /// It has to wait. With a slot, that slot holds the operation it waits
  /// for, and takes the model.
  Waiting(M, Option<u64>),
  /// It was not polled: every slot is taken.
  Skipped(M),
}

/// Counts one model being polled, until dropped.
struct Polling<'a>(&'a Lio);

impl Polling<'_> {
  /// The poll is over, with the driver borrowed.
  fn leave(self, inner: &mut LioInner) {
    inner.polling -= 1;
    std::mem::forget(self);
  }
}

// For a poll that unwinds, or ends with the driver not borrowed.
impl Drop for Polling<'_> {
  fn drop(&mut self) {
    if let Ok(mut inner) = self.0.inner.try_borrow_mut() {
      inner.polling -= 1;
    }
  }
}

/// What the backend made of an operation it was offered before it had a
/// slot.
pub(crate) enum Attempt {
  /// Performed: this is its result.
  Done(Completion),
  /// It has to be started.
  Declined,
  /// The model has had its share of results for this poll.
  NotOffered,
}

/// The sequence number of a slot's first operation.
const FIRST_OP: u64 = 1;

/// How many operations one poll of a model may have finished as they are
/// started. See [`Lio::finish_started`].
const STARTED_PER_POLL: u32 = 32;

/// What a queued submission and its consumer tell each other.
#[derive(Default)]
pub(crate) struct QueueToken {
  /// Set by the consumer: do not submit this.
  abandoned: Cell<bool>,
  /// Set by the driver once the submission has a slot: the slot, and its
  /// sequence number there.
  at: Cell<Option<(u64, u64)>>,
  /// Set by a model that asked for the result while the submission was
  /// still queued: the slot of that model, which is the one to poll.
  waiter: Cell<Option<u64>>,
}

impl OpRef {
  /// The slot and sequence number, once the operation has a slot.
  fn resolved(&self) -> Option<(u64, u64)> {
    match self {
      OpRef::Finished | OpRef::Unplaced(_) => None,
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

/// How long a dropped driver waits for its backend to report what is still
/// in flight, before it leaks that memory instead.
const SHUTDOWN_GRACE: Duration = Duration::from_millis(500);

/// How long one wait of that shutdown blocks before the driver asks again.
const SHUTDOWN_STEP: Duration = Duration::from_millis(10);

impl LioInner {
  /// Asks the backend to finish everything it has not reported, and waits
  /// up to [`SHUTDOWN_GRACE`] for those completions.
  ///
  /// The backend is told nothing it is not told while the driver runs: a
  /// cancel for each operation, then waits. What it has not reported when
  /// this returns still holds its lease.
  fn stop_backend(&mut self) {
    // Set at the first wait: the grace is time spent waiting on the
    // backend, not time the driver took to ask.
    let mut deadline = None;
    let mut held = Vec::new();
    loop {
      // Completions the driver never routed hold a share of their
      // operations' leases. Nothing reads them any more.
      self.completed.clear();
      self.ready_io.clear();
      held.clear();
      self.store.leases_held(&mut held);
      if held.is_empty() {
        return;
      }
      // Asked again each round: a backend may only be able to stop the step
      // an operation is at, and the operation has moved on since, or the
      // request may have come before there was anything to stop.
      for &id in &held {
        self.io.cancel(id);
      }
      let now = Instant::now();
      let remaining = deadline
        .get_or_insert(now + SHUTDOWN_GRACE)
        .saturating_duration_since(now);
      if remaining.is_zero() {
        return;
      }
      let mut timeout = remaining.min(SHUTDOWN_STEP);
      // Then whatever else is ready, so the next round asks about less.
      loop {
        if self.io.wait(Some(timeout), &mut self.completed).is_err() {
          return;
        }
        if self.completed.is_empty() {
          break;
        }
        self.completed.clear();
        timeout = Duration::ZERO;
      }
    }
  }
}

impl Drop for LioInner {
  fn drop(&mut self) {
    // They point into the store's arenas, so they go before it.
    drop(std::mem::take(&mut self.dying));
    self.stop_backend();
    self.completed.clear();
    self.ready_io.clear();
    // A lease still held belongs to an operation the backend has not
    // reported: it may still be running in the kernel or on a backend
    // thread, writing into buffers owned by the store and lowered state in
    // the table's arenas. Leaking both keeps that memory valid, and keeps
    // the backend from waiting again in its own `Drop`.
    if !self.store.any_lease_held() {
      // SAFETY: this field is not used again.
      let io = unsafe { ManuallyDrop::take(&mut self.io) };
      // The step arenas hold the operations' memory, which registrations
      // point into: they go after the store, and stay if the store does.
      let (backend, arenas) = io.into_parts();
      drop(backend);
      // `leaked` counts registrations forgotten while a backend still held
      // their lease. Their arenas live in the store, so the store stays.
      if self.leaked == 0 {
        #[cfg(test)]
        STORE_DROPPED.with(|dropped| dropped.set(true));
        // SAFETY: every registration was dropped, and this field is not
        // used again.
        unsafe { ManuallyDrop::drop(&mut self.store) };
        drop(arenas);
      } else {
        std::mem::forget(arenas);
      }
    }

    let Some(profile) = &self.profile else {
      return;
    };
    eprintln!(
      "\nlio-profile run_inner_calls={} completions_returned={} stale_completions={} timer_expirations={} remove_count={} next_deadline_ms={:.3} wait_ms={:.3} completion_loop_ms={:.3} completion_store_lookup_ms={:.3} completion_on_completion_ms={:.3} completion_is_finished_ms={:.3} timer_poll_ms={:.3} timer_loop_ms={:.3} remove_ms={:.3}",
      profile.run_inner_calls,
      profile.completions_returned,
      profile.stale_completions,
      profile.timer_expirations,
      profile.remove_count,
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
  /// A handle to no driver.
  pub(crate) fn none() -> Self {
    Self { inner: Weak::new() }
  }

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
  pub fn new_with_backend<D>(backend: D, cap: usize) -> io::Result<Self>
  where
    D: IoBackend + 'static,
  {
    if cap == 0 {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "lio capacity must be at least 1",
      ));
    }
    let backend: Box<dyn IoBackend> = Box::new(backend);
    let mut io = Tracked::new(backend);
    io.init(cap)?;

    let inner = LioInner {
      resource_close: io.backend().resource_close(),
      io: ManuallyDrop::new(io),
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
      started_left: 0,
      polling: 0,
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
    M: Drive + 'static,
    F: FnMut(M::Item) -> bool + 'static,
  {
    self.spawn_in(move |arena| Registration::running(arena, model, sink), None)
  }

  /// Runs `model` for a future: its item waits in the slot until
  /// [`take_awaited`](Self::take_awaited) returns it.
  ///
  /// `waker` is woken when the item is there, unless it is already when
  /// this returns.
  ///
  /// The slot is not given back before the item is taken or
  /// [`drop_awaited`](Self::drop_awaited) is called.
  pub(crate) fn spawn_awaited<M>(&self, model: M, waker: &Waker) -> Spawned
  where
    M: Drive + 'static,
  {
    self.spawn_in(move |arena| Registration::awaited(arena, model), Some(waker))
  }

  /// The item of a model started with
  /// [`spawn_awaited`](Self::spawn_awaited), once, when it is there. Until
  /// then, `waker` is what is woken when it is.
  ///
  /// # Safety
  ///
  /// `spawned` came from a `spawn_awaited` on this driver, of a model whose
  /// item is `T`, and [`drop_awaited`](Self::drop_awaited) was not called
  /// for it.
  pub(crate) unsafe fn take_awaited<T>(
    &self,
    spawned: &Spawned,
    waker: &Waker,
  ) -> Option<T> {
    let id = spawned.id()?;
    let mut inner = self.inner.borrow_mut();
    let inner_ref = &mut *inner;
    let reg = inner_ref.store.get_mut(id)?;
    // SAFETY: a slot id is never reused with the same generation, so `id`
    // names the slot `spawn_awaited` filled, for a model whose item is `T`.
    match unsafe { reg.take_mail::<T>(waker) } {
      Mail::Item(item, waker) => {
        // Still running when its own waker polled the future: whoever
        // polls the model gives the slot back.
        let spent = reg.is_spent();
        if spent {
          Self::retire(inner_ref, id);
        }
        let settle = spent && Self::slot_is_wanted(inner_ref);
        drop(inner);
        // A waker's drop is the caller's code.
        drop(waker);
        if settle {
          // The slot goes to a model that was waiting for one, which is
          // polled now: no run is under way to do it.
          self.bury_and_wake();
          self.poll_woken();
        }
        Some(item)
      }
      Mail::Waiting(replaced) => {
        drop(inner);
        // A waker's drop is the caller's code.
        drop(replaced);
        None
      }
    }
  }

  /// The future of a model started with
  /// [`spawn_awaited`](Self::spawn_awaited) is gone: the model is stopped,
  /// an item nobody took is dropped, and the slot is given back.
  ///
  /// # Safety
  ///
  /// As for [`take_awaited`](Self::take_awaited). Called at most once.
  pub(crate) unsafe fn drop_awaited<T>(&self, spawned: &Spawned) {
    if let Spawned::Queued(token) = spawned {
      // Still queued: it never starts.
      token.abandon();
    }
    let Some(id) = spawned.id() else { return };
    let (mail, spent) = {
      let mut inner = self.inner.borrow_mut();
      let inner_ref = &mut *inner;
      let Some(reg) = inner_ref.store.get_mut(id) else { return };
      // SAFETY: as in `take_awaited`.
      let mail = unsafe { reg.release_mail::<T>() };
      let spent = reg.is_spent();
      if spent {
        Self::retire(inner_ref, id);
      }
      (mail, spent && Self::slot_is_wanted(inner_ref))
    };
    if spent {
      // As in `take_awaited`.
      self.bury_and_wake();
      self.poll_woken();
    }
    // The item and the waker are the caller's: dropped with the driver not
    // borrowed.
    drop(mail);
    // A model that is still running is dropped, and its slot given back.
    self.cancel_task(id);
  }

  /// Whether giving a slot back leaves anything to do: something waits for
  /// a slot, or for the driver to be free of its borrow.
  #[inline]
  fn slot_is_wanted(inner: &LioInner) -> bool {
    !(inner.overflow.is_empty()
      && inner.capacity_waiters.is_empty()
      && inner.dying.is_empty()
      && inner.abandoned.is_empty()
      && inner.woken.is_empty())
  }

  /// Fills a slot with what `make` builds in its arena, and polls its model
  /// for the first time. Over capacity it waits in a queue.
  ///
  /// `mail` is what wakes the future of a model that runs for one.
  fn spawn_in(
    &self,
    make: impl FnOnce(&mut Bump) -> Registration + 'static,
    mail: Option<&Waker>,
  ) -> Spawned {
    let mut inner = self.inner.borrow_mut();
    // Queued submissions go first.
    if !inner.overflow.is_empty() || inner.store.is_full() {
      let token = Rc::new(QueueToken::default());
      let mail = mail.cloned();
      let start = move |inner: &mut LioInner, _home: Option<u64>| {
        let id = Self::insert_task(inner, make);
        if let Some(waker) = mail
          && let Some(reg) = inner.store.get_mut(id)
        {
          reg.arm_mail(waker);
        }
        Self::wake(inner, id);
        (id, FIRST_OP)
      };
      Self::enqueue(&mut inner, start, Some(Rc::clone(&token)), None);
      return Spawned::Queued(token);
    }
    let id = Self::insert_task(&mut inner, make);
    let begun = Self::begin(&mut inner, id);
    let Some(begun) = begun else {
      unreachable!("driver: a model that was just started cannot be polled");
    };
    let (mut inner, freed) = self.poll_begun(inner, id, begun);
    // Armed after the poll, so that a model done already has woken
    // nothing: whoever spawned it asks for its item next.
    if let Some(waker) = mail
      && let Some(reg) = inner.store.get_mut(id)
    {
      reg.arm_mail(waker.clone());
    }
    drop(inner);
    if freed {
      self.bury_and_wake();
    }
    Spawned::Now(id)
  }

  /// Stops a model started with [`spawn`](Self::spawn): it is dropped, and
  /// what it has in flight is cancelled.
  pub(crate) fn cancel(&self, spawned: &Spawned) {
    let id = match spawned {
      Spawned::Done => None,
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
  fn insert_task(
    inner: &mut LioInner,
    make: impl FnOnce(&mut Bump) -> Registration,
  ) -> u64 {
    let inserted =
      inner.store.insert_submitting(make, |_id, _reg, _lease, insert| {
        insert.keep_on_panic()
      });
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
      if freed {
        // Slots went back: that may start models that were waiting for
        // one, which are polled next.
        drop(inner);
        self.bury_and_wake();
        inner = self.inner.borrow_mut();
      } else {
        return;
      }
    }
  }

  /// Marks the model in slot `id` busy and returns what polls it, or what
  /// drops it if it was asked to stop while it could not be dropped.
  #[inline]
  fn begin(inner: &mut LioInner, id: u64) -> Option<Begun> {
    Self::begin_in(inner.store.get_mut(id)?)
  }

  /// [`begin`](Self::begin) for a slot that is in hand.
  #[inline]
  fn begin_in(reg: &mut Registration) -> Option<Begun> {
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
    let polling = Self::enter_poll(self, &mut inner);
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
        AfterPoll::Pending => {
          polling.leave(inner_ref);
          return (inner, false);
        }
        AfterPoll::Finished => {
          let spent = reg.is_spent();
          if spent {
            Self::retire(inner_ref, id);
          }
          polling.leave(inner_ref);
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
  pub(crate) fn submit_op(
    &self,
    task: u64,
    place: impl PlaceOp,
    attempted: bool,
  ) -> OpRef {
    let inner = self.inner.borrow_mut();
    self.submit_at(inner, task, place, attempted)
  }

  /// [`submit_op`](Self::submit_op) for a model that is polled before it
  /// has a slot. `home` is the slot it will get: the slot of the first
  /// operation it started that has to wait, once there is one.
  pub(crate) fn submit_detached(
    &self,
    home: &mut Option<u64>,
    place: impl PlaceOp,
    attempted: bool,
  ) -> OpRef {
    let mut inner = self.inner.borrow_mut();
    // Its operation there may be over, and the slot given back.
    if home.is_some_and(|home| inner.store.get_mut(home).is_none()) {
      *home = None;
    }
    if let Some(home) = *home {
      return self.submit_at(inner, home, place, attempted);
    }
    if !inner.overflow.is_empty() || inner.store.is_full() {
      // Queued, it could get the slot its model then waits for.
      return OpRef::Unplaced(Some(Box::new(place)));
    }
    // The operation's slot is the model's, so the model is the one polled
    // when it is over.
    let id = Self::insert_op(&mut inner, None, place, attempted);
    *home = Some(id);
    self.finish_started(inner, id);
    OpRef::At { id, seq: FIRST_OP }
  }

  /// Polls `model` for the first time, here, before it has a slot. What
  /// the backend performs as the model starts it needs none, and a model
  /// that is done after that never had one.
  ///
  /// Items go to `sink`, as in a slot.
  pub(crate) fn trial<M: Drive>(
    &self,
    mut model: M,
    sink: &mut impl FnMut(M::Item) -> bool,
  ) -> Trial<M> {
    let polling = {
      let mut inner = self.inner.borrow_mut();
      if !inner.overflow.is_empty() || inner.store.is_full() {
        return Trial::Skipped(model);
      }
      Self::enter_poll(self, &mut inner)
    };
    let mut cx = Cx::detached(self);
    loop {
      match model.drive(&mut cx) {
        Turn::Pending => {
          drop(polling);
          let home = cx.home().filter(|&home| {
            self.inner.borrow_mut().store.get_mut(home).is_some()
          });
          return Trial::Waiting(model, home);
        }
        Turn::Yield(item) => {
          if !sink(item) {
            return Trial::Finished;
          }
        }
        Turn::End => return Trial::Finished,
        Turn::Done(item) => {
          // Dropped before its item is delivered, as in a slot.
          drop(model);
          sink(item);
          return Trial::Finished;
        }
      }
    }
  }

  /// Gives slot `home`, which holds the operation a model waits for, that
  /// model: what `make` builds in the slot's arena. `mail` is what wakes
  /// the future of a model that runs for one.
  pub(crate) fn adopt(
    &self,
    home: u64,
    make: impl FnOnce(&mut Bump) -> Registration,
    mail: Option<&Waker>,
  ) -> Spawned {
    let mut inner = self.inner.borrow_mut();
    let adopted = inner.store.adopt(home, make);
    assert!(adopted, "driver: the slot a model waits in was given back");
    if let Some(waker) = mail
      && let Some(reg) = inner.store.get_mut(home)
    {
      reg.arm_mail(waker.clone());
    }
    Spawned::Now(home)
  }

  /// Counts a poll that is starting. The first of a nest gets the share of
  /// results models may have at their submits: polls inside it (a callback
  /// that submits, whose callback submits) draw on the same share, so they
  /// cannot nest without end.
  fn enter_poll<'a>(lio: &'a Lio, inner: &mut LioInner) -> Polling<'a> {
    if inner.polling == 0 {
      inner.started_left = STARTED_PER_POLL;
    }
    inner.polling += 1;
    Polling(lio)
  }

  fn submit_at(
    &self,
    mut inner: RefMut<'_, LioInner>,
    task: u64,
    place: impl PlaceOp,
    attempted: bool,
  ) -> OpRef {
    let place = match Self::start_in_slot(&mut inner, task, place, attempted) {
      Ok(seq) => {
        self.finish_started(inner, task);
        return OpRef::At { id: task, seq };
      }
      Err(place) => place,
    };
    if !inner.overflow.is_empty() || inner.store.is_full() {
      let token = Rc::new(QueueToken::default());
      let start = move |inner: &mut LioInner, home: Option<u64>| {
        let Some(home) = home else {
          let id = Self::insert_op(inner, Some(task), place, attempted);
          return (id, FIRST_OP);
        };
        match Self::start_in_slot(inner, home, place, attempted) {
          Ok(seq) => (home, seq),
          Err(_) => unreachable!(
            "driver: a queued operation was sent to a slot that holds one"
          ),
        }
      };
      Self::enqueue(&mut inner, start, Some(Rc::clone(&token)), Some(task));
      return OpRef::Queued(token);
    }
    let id = Self::insert_op(&mut inner, Some(task), place, attempted);
    self.finish_started(inner, id);
    OpRef::At { id, seq: FIRST_OP }
  }

  /// Offers the backend `op` before it has a slot or a place in the table.
  ///
  /// `op` borrows its memory for this call only: a backend that performs
  /// it is done with that memory when this returns. One poll of a model
  /// gets [`STARTED_PER_POLL`] results at its submits, from here or from
  /// [`finish_started`](Self::finish_started).
  pub(crate) fn attempt(&self, op: &crate::backend::op::Op) -> Attempt {
    let mut inner = self.inner.borrow_mut();
    if inner.started_left == 0 {
      return Attempt::NotOffered;
    }
    let Some(result) = inner.io.attempt(op) else {
      return Attempt::Declined;
    };
    inner.started_left -= 1;
    Attempt::Done(
      Completion::new(result).with_resource_close(inner.resource_close),
    )
  }

  /// Reads the result of operation `id`, which was just started, into its
  /// slot, if the backend finished it there and then. The model that
  /// submitted it is being polled, and takes the result next.
  ///
  /// One poll of a model gets [`STARTED_PER_POLL`] results this way. After
  /// that they wait for the next run like any other, so a model whose
  /// operations never wait still lets the rest of the program run.
  fn finish_started(&self, mut inner: RefMut<'_, LioInner>, id: u64) {
    if inner.started_left == 0 {
      return;
    }
    let Some(done) = inner.io.take_started(id) else {
      return;
    };
    inner.started_left -= 1;
    let completion =
      Completion::new(done.result()).with_resource_close(inner.resource_close);
    // The table's share of the operation's memory goes with its report.
    drop(done);
    let begun = inner.store.get_mut(id).and_then(|reg| reg.begin_completion());
    let Some(raw) = begun else {
      unreachable!("driver: operation {id} was started and is not in flight");
    };
    drop(inner);
    let mut batch = Batch { lio: self, current: Some(id) };
    // SAFETY: the registration is alive and its operation was just marked
    // busy, which keeps the slot from being given back.
    unsafe { raw.complete(completion) };
    batch.current = None;
    let mut inner = self.inner.borrow_mut();
    let Some(reg) = inner.store.get_mut(id) else {
      panic!("driver: slot {id} was given back while its result was read");
    };
    // Its ticket is still being made: the result waits for it.
    let _waiter = reg.end_completion();
  }

  /// Starts an operation in slot `id`, which is taken already. Gives
  /// `state` back when that slot cannot hold another operation right now.
  fn start_in_slot<P: PlaceOp>(
    inner: &mut LioInner,
    id: u64,
    place: P,
    attempted: bool,
  ) -> Result<u64, P> {
    let LioInner { io, store, time, .. } = inner;
    let started = store.with_slot(id, move |reg, lease| {
      // A backend that still holds the lease may still use the memory of
      // the operation before this one.
      if !reg.can_start_op() || lease.held_elsewhere() {
        return Err(place);
      }
      // The operation before this one is finished and its result taken:
      // nothing points into the arena any more.
      let step_bump = io.arena(id);
      step_bump.reset();
      let seq = place(reg, step_bump, id);
      let mut abort = AbortOp { reg, armed: true };
      let AbortOp { reg, armed } = &mut abort;
      reg.with_action(&mut |action| {
        // After this, a panic must not free the operation's memory: the
        // table has a copy of its addresses. A panic while building the action
        // happens first, and frees it.
        *armed = false;
        match action {
          Action::Io(op) => io.push_model(id, op, lease.clone(), attempted),
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
  /// `waiter`, or for one that will be given this very slot. The store
  /// must have a free slot.
  fn insert_op(
    inner: &mut LioInner,
    waiter: Option<u64>,
    place: impl PlaceOp,
    attempted: bool,
  ) -> u64 {
    let LioInner { io, store, time, .. } = inner;
    let inserted = store.insert_submitting(
      |_arena| Registration::vacant(),
      |id, reg, lease, insert| {
        // The slot's last operation is long gone: nothing points into the
        // arena any more.
        let step_bump = io.arena(id);
        step_bump.reset();
        place(reg, step_bump, waiter.unwrap_or(id));
        reg.with_action(&mut |action| {
          // After this, a panic must not free the operation's memory: the
          // table has a copy of its addresses. A panic while building
          // the action happens first and still frees the slot.
          insert.keep_on_panic();
          match action {
            Action::Io(op) => io.push_model(id, op, lease.clone(), attempted),
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
    let Some((id, seq)) = op.resolved() else {
      // Still waiting for a slot, and maybe handed to another model since
      // it was submitted: the one asking now is the one to poll.
      if let OpRef::Queued(token) = op {
        token.waiter.set(Some(waiter));
      }
      return None;
    };
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

  /// A queued submission now has a slot: tells its consumer where, and the
  /// slot which model asked for the result meanwhile.
  fn left_the_queue(
    inner: &mut LioInner,
    token: Option<Rc<QueueToken>>,
    at: (u64, u64),
  ) {
    let Some(token) = token else { return };
    token.at.set(Some(at));
    if let Some(waiter) = token.waiter.take()
      && let Some(reg) = inner.store.get_mut(at.0)
    {
      reg.set_waiter(waiter);
    }
  }

  /// Slot `home` can hold an operation again: starts there the first
  /// operation its model has waiting for a slot, if any.
  #[inline]
  fn start_waiting_at_home(inner: &mut LioInner, home: u64) {
    // Nearly always: nothing is waiting for a slot.
    if inner.overflow.is_empty() {
      return;
    }
    Self::start_queued_at_home(inner, home);
  }

  #[cold]
  #[inline(never)]
  fn start_queued_at_home(inner: &mut LioInner, home: u64) {
    // A backend that still holds the lease may still use the memory of the
    // operation that was here.
    if inner.store.lease_held(home) {
      return;
    }
    loop {
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
      Self::left_the_queue(inner, token, at);
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
      Self::left_the_queue(inner, token, at);
    }
    if inner.store.is_full() {
      Vec::new()
    } else {
      std::mem::take(&mut inner.capacity_waiters)
    }
  }

  /// Non-blocking poll for completed operations.
  ///
  /// Returns immediately, processing any completions that are ready, and
  /// how many that was. An operation the backend finished as it was
  /// submitted was delivered then, and is not counted here.
  #[inline]
  pub fn try_run(&self) -> io::Result<usize> {
    self.run_inner(Some(Duration::ZERO))
  }

  /// Block until at least one operation completes.
  ///
  /// Returns at once, with 0, when nothing is in flight and no timer is
  /// set: an operation the backend finished as it was submitted was
  /// delivered then, and there is nothing to wait for.
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
    // Nothing in flight, nothing queued, no timer: a turn would bring
    // nothing. Only a run that was asked to wait a while still does.
    if timeout.is_none_or(|timeout| timeout.is_zero()) && Self::is_idle(&inner)
    {
      return Ok(0);
    }
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

  /// Whether the driver has nothing in flight and nothing left to do.
  #[inline]
  fn is_idle(inner: &LioInner) -> bool {
    inner.io.is_idle()
      && inner.time.is_empty()
      && inner.ready_io.is_empty()
      && inner.ready_timers.is_empty()
      && !Self::slot_is_wanted(inner)
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
      let mut inner = self.inner.borrow_mut();
      if !inner.abandoned.is_empty() {
        // Queued submissions nobody waits for any more hold user values:
        // dropped here, with the driver not borrowed.
        let abandoned = std::mem::take(&mut inner.abandoned);
        drop(inner);
        drop(abandoned);
        inner = self.inner.borrow_mut();
      }
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
        // Only a result that could not be read leaves a registration here,
        // so there are never many: their slots go back in one borrow.
        while let Some(Detached { registration, slot }) = batch.pending.pop() {
          batch.dropping = Some(slot);
          drop(registration);
          batch.dropping = None;
          batch.slots.push(slot);
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
    // One at a time is the common case, and far cheaper moved by hand than
    // through `extend`.
    if inner_ref.completed.len() == 1 {
      if let Some(only) = inner_ref.completed.pop() {
        inner_ref.ready_io.push_back(only);
      }
    } else {
      inner_ref.ready_io.extend(inner_ref.completed.drain(..));
    }
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
    match reg.end_completion() {
      // The model that was waiting for it takes it now. A model that is
      // busy further up the stack is polled again when it is not.
      Some(waiter) => {
        // Usually its own slot's model, which is in hand already.
        let begun = if waiter == id {
          Self::begin_in(reg)
        } else {
          Self::begin(inner_ref, waiter)
        };
        if let Some(begun) = begun {
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
  #[inline]
  fn drop(&mut self) {
    if let Some(id) = self.current.take() {
      self.unwound(id);
    }
  }
}

impl Batch<'_> {
  /// The handler of operation `id` panicked.
  #[cold]
  #[inline(never)]
  fn unwound(&mut self, id: u64) {
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
  #[inline]
  fn drop(&mut self) {
    if self.armed {
      self.unwound();
    }
  }
}

impl PollGuard<'_> {
  /// The model panicked while it was polled.
  #[cold]
  #[inline(never)]
  fn unwound(&mut self) {
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
  #[inline]
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

/// A [`ScriptedBackend`](crate::backend::testing::ScriptedBackend) as a test
/// scripts it: the kernel under a driver.
///
/// The driver hands the backend an operation as it is submitted, so the
/// kernel has whatever a test has submitted.
#[cfg(test)]
#[derive(Clone)]
pub(crate) struct Kernel {
  backend: crate::backend::testing::ScriptedBackend,
}

#[cfg(test)]
impl Kernel {
  /// The kernel under a driver that was made with a clone of `backend`.
  pub(crate) fn new(backend: crate::backend::testing::ScriptedBackend) -> Self {
    Self { backend }
  }

  /// A driver over a scripted backend, and its kernel.
  pub(crate) fn with_lio(cap: usize) -> (Self, Lio) {
    let backend = crate::backend::testing::ScriptedBackend::new();
    let lio = match Lio::new_with_backend(backend.clone(), cap) {
      Ok(lio) => lio,
      Err(err) => panic!("a scripted backend initializes: {err}"),
    };
    (Self::new(backend.clone()), lio)
  }

  pub(crate) fn pushed(&self) -> Vec<u64> {
    self.backend.pushed()
  }

  pub(crate) fn in_flight(&self) -> Vec<u64> {
    self.backend.in_flight()
  }

  pub(crate) fn op(&self, id: u64) -> Option<crate::backend::op::Op> {
    self.backend.op(id)
  }

  pub(crate) fn cancelled(&self) -> Vec<u64> {
    self.backend.cancelled()
  }

  pub(crate) fn complete(&self, id: u64, result: isize) {
    self.backend.complete(id, result);
  }

  pub(crate) fn complete_all(&self, result: isize) {
    self.backend.complete_all(result);
  }

  pub(crate) fn set_stuck(&self, stuck: bool) {
    self.backend.set_stuck(stuck);
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::backend::testing::ScriptedBackend;
  use crate::backend::{Done, OpKey, Queued, Started};

  fn lio(cap: usize) -> (Kernel, Lio) {
    Kernel::with_lio(cap)
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
  #[cfg_attr(miri, ignore = "leaks the registration on purpose")]
  fn an_operation_never_reported_keeps_the_store_after_the_driver_drops() {
    // `Hoarding` ignores the cancel, and never reports what it began.
    let lio = Lio::new_with_backend(Hoarding::default(), 1).unwrap();
    crate::api::nop().with_lio(&lio).when_done(|_| {});
    // The backend has to have it: one that is still queued is the table's
    // to give up.
    assert_eq!(lio.inner.borrow().leaked, 0);
    STORE_DROPPED.with(|dropped| dropped.set(false));
    drop(lio);
    assert!(
      !STORE_DROPPED.with(|dropped| dropped.get()),
      "the store was freed under an operation the backend never gave up"
    );
  }

  #[test]
  #[cfg_attr(not(panic = "unwind"), ignore = "catches a panic")]
  fn a_completion_never_routed_does_not_keep_the_store() {
    let lio = Lio::new_with_backend(ScriptedBackend::auto(0), 2).unwrap();
    crate::api::nop().with_lio(&lio).when_done(|_| panic!("handler failure"));
    crate::api::nop().with_lio(&lio).when_done(|_| {});
    let run =
      std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| lio.try_run()));
    assert!(run.is_err(), "the first handler panics");
    assert_eq!(lio.inner.borrow().ready_io.len(), 1, "one is left queued");
    STORE_DROPPED.with(|dropped| dropped.set(false));
    drop(lio);
    assert!(STORE_DROPPED.with(|dropped| dropped.get()));
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
  fn dropping_the_driver_stops_what_nobody_cancelled() {
    let (backend, lio) = lio(2);
    // Their consumers are alive, so nothing has asked to cancel them.
    let _first = crate::api::nop().with_lio(&lio).send();
    let _second = crate::api::nop().with_lio(&lio).send();
    let ids = backend.pushed();
    assert_eq!(ids.len(), 2);
    assert!(backend.cancelled().is_empty());
    STORE_DROPPED.with(|dropped| dropped.set(false));
    drop(lio);
    assert_eq!(backend.cancelled(), ids, "each is asked once");
    assert!(backend.in_flight().is_empty());
    assert!(STORE_DROPPED.with(|dropped| dropped.get()));
  }

  #[test]
  #[cfg_attr(miri, ignore = "leaks the store on purpose")]
  fn dropping_the_driver_keeps_asking_until_its_grace_is_over() {
    let (backend, lio) = lio(1);
    let _held = crate::api::nop().with_lio(&lio).send();
    backend.set_stuck(true);
    STORE_DROPPED.with(|dropped| dropped.set(false));
    let started = Instant::now();
    drop(lio);
    assert!(started.elapsed() >= SHUTDOWN_GRACE);
    assert!(backend.cancelled().len() > 1, "asked again while it waited");
    assert_eq!(backend.in_flight().len(), 1);
    assert!(
      !STORE_DROPPED.with(|dropped| dropped.get()),
      "the store was freed under an operation the backend never reported"
    );
  }

  #[test]
  fn dropping_a_driver_with_nothing_in_flight_frees_the_store() {
    let (backend, lio) = lio(1);
    crate::api::nop().with_lio(&lio).when_done(|_| {});
    let id = backend.pushed()[0];
    backend.complete(id, 0);
    lio.try_run().unwrap();
    STORE_DROPPED.with(|dropped| dropped.set(false));
    drop(lio);
    assert!(STORE_DROPPED.with(|dropped| dropped.get()));
  }

  #[test]
  #[should_panic(expected = "which is not in flight")]
  fn a_second_completion_for_an_operation_panics() {
    let (backend, lio) = lio(1);
    crate::api::nop().with_lio(&lio).when_done(|_| {});
    lio.try_run().unwrap();
    let id = backend.pushed()[0];
    backend.complete(id, 0);
    lio.try_run().unwrap();
    // The table reports an operation once: the second report is made up.
    lio.inner.borrow_mut().io.forge(id, 0);
    lio.try_run().unwrap();
  }

  #[test]
  fn a_timer_firing_after_its_sleep_was_cancelled_is_ignored() {
    use std::cell::RefCell;
    use std::future::{Future, IntoFuture};
    use std::rc::Rc;
    use std::task::Context;

    let (_, lio) = lio(3);
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
    // Fires in that run too, after both.
    let fired = Rc::new(Cell::new(false));
    let set = fired.clone();
    crate::api::sleep(Duration::from_millis(3))
      .with_lio(&lio)
      .when_done(move |_| set.set(true));
    lio.advance_time_by_ticks(5);
    lio.try_run().unwrap();
    assert!(later.borrow().is_none());
    assert!(fired.get(), "only the dropped sleep's firing is forgotten");
    assert!(!lio.is_full());
  }

  #[test]
  fn an_awaited_result_keeps_its_slot_until_the_future_takes_it() {
    use std::future::{Future, IntoFuture};
    use std::task::{Context, Poll};

    let (backend, lio) = lio(1);
    let mut nop = Box::pin(crate::api::nop().with_lio(&lio).into_future());
    let mut cx = Context::from_waker(Waker::noop());
    assert!(nop.as_mut().poll(&mut cx).is_pending());
    backend.complete_all(0);
    assert_eq!(lio.try_run().unwrap(), 1);
    assert!(lio.is_full(), "the result waits in the slot");
    assert!(matches!(nop.as_mut().poll(&mut cx), Poll::Ready(Ok(()))));
    assert!(!lio.is_full(), "taking the result gives the slot back");
  }

  #[test]
  fn dropping_a_future_gives_back_the_slot_of_a_result_it_never_took() {
    use std::future::{Future, IntoFuture};
    use std::task::Context;

    let (backend, lio) = lio(1);
    let mut nop = Box::pin(crate::api::nop().with_lio(&lio).into_future());
    let mut cx = Context::from_waker(Waker::noop());
    assert!(nop.as_mut().poll(&mut cx).is_pending());
    backend.complete_all(0);
    assert_eq!(lio.try_run().unwrap(), 1);
    assert!(lio.is_full());
    drop(nop);
    assert!(!lio.is_full());
  }

  #[test]
  fn a_future_outlives_a_driver_that_held_its_result() {
    use std::future::{Future, IntoFuture};
    use std::task::Context;

    let (backend, lio) = lio(1);
    let mut nop = Box::pin(crate::api::nop().with_lio(&lio).into_future());
    let mut cx = Context::from_waker(Waker::noop());
    assert!(nop.as_mut().poll(&mut cx).is_pending());
    backend.complete_all(0);
    assert_eq!(lio.try_run().unwrap(), 1);
    // The result goes with the driver.
    drop(lio);
    assert!(nop.as_mut().poll(&mut cx).is_pending());
  }

  #[test]
  fn a_waker_may_poll_its_future_as_it_is_woken() {
    use std::cell::RefCell;
    use std::future::{Future, IntoFuture};
    use std::pin::Pin;
    use std::sync::Arc;
    use std::task::{Context, Poll, Wake};

    type Nop = Pin<Box<dyn Future<Output = io::Result<()>>>>;
    thread_local! {
      static WAITING: RefCell<Option<Nop>> = const { RefCell::new(None) };
      static GOT: Cell<bool> = const { Cell::new(false) };
    }
    /// Polls the future the moment it is woken.
    struct Eager;
    impl Wake for Eager {
      fn wake(self: Arc<Self>) {
        let Some(mut nop) = WAITING.with(|waiting| waiting.borrow_mut().take())
        else {
          return;
        };
        let mut cx = Context::from_waker(Waker::noop());
        assert!(matches!(nop.as_mut().poll(&mut cx), Poll::Ready(Ok(()))));
        GOT.with(|got| got.set(true));
      }
    }

    let (backend, lio) = lio(1);
    let mut nop: Nop = Box::pin(crate::api::nop().with_lio(&lio).into_future());
    let waker = Waker::from(Arc::new(Eager));
    let mut cx = Context::from_waker(&waker);
    assert!(nop.as_mut().poll(&mut cx).is_pending());
    WAITING.with(|waiting| *waiting.borrow_mut() = Some(nop));
    backend.complete_all(0);
    assert_eq!(lio.try_run().unwrap(), 1);
    assert!(GOT.with(Cell::get), "the waker polled the future");
    assert!(!lio.is_full(), "the slot went back once its model was done");
  }

  /// Finishes every nop as it is started.
  struct AtOnce;

  impl IoBackend for AtOnce {
    fn init(&mut self, _: usize) -> io::Result<()> {
      Ok(())
    }
    fn resource_close(&self) -> crate::api::resource::ResourceClose {
      crate::api::resource::ResourceClose::noop()
    }
    fn start(
      &mut self,
      op: crate::backend::Queued<'_>,
      _: &mut Bump,
      done: &mut crate::backend::Done<'_>,
    ) -> crate::backend::Started {
      let (started, key, _) = op.begin();
      // SAFETY: a nop has no addresses, and 0 is what a nop does.
      unsafe { done.complete(key, 0) };
      started
    }
    fn cancel(
      &mut self,
      _: crate::backend::OpKey,
      _: &mut crate::backend::Done<'_>,
    ) {
    }
    fn wait(
      &mut self,
      _: Option<Duration>,
      _: &mut crate::backend::Done<'_>,
    ) -> io::Result<()> {
      Ok(())
    }
  }

  #[test]
  fn a_callback_runs_at_the_submit_the_backend_finished() {
    let lio = Lio::new_with_backend(AtOnce, 1).unwrap();
    let done = Rc::new(Cell::new(false));
    let set = done.clone();
    crate::api::nop().with_lio(&lio).when_done(move |result| {
      result.unwrap();
      set.set(true);
    });
    assert!(done.get(), "no run was needed");
    assert!(!lio.is_full(), "and the slot is free again");
    assert_eq!(lio.try_run().unwrap(), 0, "nothing is left to route");
  }

  #[test]
  fn a_future_is_ready_on_the_poll_the_backend_finished_it_in() {
    use std::future::{Future, IntoFuture};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::task::{Context, Poll, Wake};

    struct Flag(AtomicBool);
    impl Wake for Flag {
      fn wake(self: Arc<Self>) {
        self.0.store(true, Ordering::Relaxed);
      }
    }

    let lio = Lio::new_with_backend(AtOnce, 1).unwrap();
    let woken = Arc::new(Flag(AtomicBool::new(false)));
    let waker = Waker::from(woken.clone());
    let mut cx = Context::from_waker(&waker);
    let mut nop = Box::pin(crate::api::nop().with_lio(&lio).into_future());
    assert!(matches!(nop.as_mut().poll(&mut cx), Poll::Ready(Ok(()))));
    assert!(!woken.0.load(Ordering::Relaxed), "it was being polled already");
    assert!(!lio.is_full());
  }

  /// Runs one nop after another: for ever, as far as a test goes.
  struct Spinner {
    nop: Nop,
    turns: Rc<Cell<u32>>,
  }

  /// Where a [`Spinner`] gives up, so that a driver that never makes it
  /// wait fails a test instead of hanging it.
  const SPINS: u32 = 10_000;

  impl OpModel for Spinner {
    type Item = ();

    fn poll(&mut self, cx: &mut Cx<'_>) -> Step<()> {
      while self.turns.get() < SPINS {
        let _ = ready!(self.nop.poll(cx));
        self.turns.set(self.turns.get() + 1);
        self.nop = Nop::new();
      }
      Step::Done(())
    }
  }

  /// Performs a nop it is offered, and holds one it is given to start
  /// until it is cancelled.
  #[derive(Default)]
  struct OnlyOffered {
    held: Vec<OpKey>,
  }

  impl IoBackend for OnlyOffered {
    fn init(&mut self, _: usize) -> io::Result<()> {
      Ok(())
    }
    fn resource_close(&self) -> crate::api::resource::ResourceClose {
      crate::api::resource::ResourceClose::noop()
    }
    fn attempt(
      &mut self,
      op: crate::backend::Offered<'_>,
    ) -> crate::backend::Attempted {
      // SAFETY: a nop has no addresses, and 0 is what a nop does.
      unsafe { op.finish(0) }
    }
    fn start(
      &mut self,
      op: Queued<'_>,
      _: &mut Bump,
      _: &mut Done<'_>,
    ) -> Started {
      let (started, key, _) = op.begin();
      self.held.push(key);
      started
    }
    fn cancel(&mut self, key: OpKey, done: &mut Done<'_>) {
      // SAFETY: a nop has no addresses, and the result is negative.
      unsafe { done.cancelled(key, -1) };
    }
    fn wait(
      &mut self,
      _: Option<Duration>,
      _: &mut Done<'_>,
    ) -> io::Result<()> {
      Ok(())
    }
  }

  #[test]
  fn every_poll_that_is_not_nested_gets_its_own_share_of_results() {
    let lio = Lio::new_with_backend(OnlyOffered::default(), 4).unwrap();
    let done = Rc::new(Cell::new(0));
    for _ in 0..3 * STARTED_PER_POLL {
      let count = done.clone();
      crate::api::nop()
        .with_lio(&lio)
        .when_done(move |_| count.set(count.get() + 1));
    }
    // The backend performed each as it was offered: none had to be started.
    assert_eq!(done.get(), 3 * STARTED_PER_POLL);
    assert!(!lio.is_full());
  }

  #[test]
  fn what_the_backend_performs_as_offered_counts_against_the_share() {
    let lio = Lio::new_with_backend(OnlyOffered::default(), 1).unwrap();
    let turns = Rc::new(Cell::new(0));
    let model = Spinner { nop: Nop::new(), turns: turns.clone() };
    let task = crate::api::run(model).with_lio(&lio).when_done(|()| {});
    // After a poll's worth, the next is started, which this backend holds.
    assert_eq!(turns.get(), STARTED_PER_POLL);
    assert!(lio.is_full(), "the model waits in a slot");
    task.cancel();
    lio.try_run().unwrap();
    assert!(!lio.is_full());
  }

  /// Says that it was polled, then runs a nop.
  struct Seen {
    polled: Rc<Cell<bool>>,
    nop: Nop,
  }

  impl OpModel for Seen {
    type Item = ();

    fn poll(&mut self, cx: &mut Cx<'_>) -> Step<()> {
      self.polled.set(true);
      let _ = ready!(self.nop.poll(cx));
      Step::Done(())
    }
  }

  #[test]
  fn a_model_is_not_polled_while_every_slot_is_taken() {
    let (backend, lio) = lio(1);
    crate::api::nop().with_lio(&lio).when_done(|_| {});
    assert!(lio.is_full());
    let polled = Rc::new(Cell::new(false));
    let model = Seen { polled: polled.clone(), nop: Nop::new() };
    crate::api::run(model).with_lio(&lio).when_done(|()| {});
    assert!(!polled.get(), "it waits for a slot first");
    backend.complete_all(0);
    lio.try_run().unwrap();
    assert!(polled.get());
    backend.complete_all(0);
    lio.try_run().unwrap();
    assert!(!lio.is_full());
  }

  /// Yields the numbers down from `left`, with a nop before each.
  struct Countdown {
    left: u32,
    nop: Nop,
  }

  impl crate::model::OpStream for Countdown {
    type Item = u32;

    fn poll_next(&mut self, cx: &mut Cx<'_>) -> crate::model::Next<u32> {
      if self.left == 0 {
        return crate::model::Next::End;
      }
      let _ = ready!(self.nop.poll(cx));
      self.nop = Nop::new();
      self.left -= 1;
      crate::model::Next::Item(self.left)
    }
  }

  #[test]
  fn a_stream_goes_on_after_an_item_it_yields_at_its_first_poll() {
    let lio = Lio::new_with_backend(AtOnce, 1).unwrap();
    let items = Rc::new(RefCell::new(Vec::new()));
    let sink = items.clone();
    let stream = Countdown { left: 3, nop: Nop::new() };
    crate::model::spawn_stream(&lio, stream, move |item| {
      sink.borrow_mut().push(item)
    });
    assert_eq!(*items.borrow(), [2, 1, 0]);
    assert!(!lio.is_full());
  }

  #[test]
  fn a_model_whose_operations_never_wait_still_returns_to_its_caller() {
    let lio = Lio::new_with_backend(AtOnce, 1).unwrap();
    let turns = Rc::new(Cell::new(0));
    let spawned =
      lio.spawn(Spinner { nop: Nop::new(), turns: turns.clone() }, |()| true);
    assert_eq!(turns.get(), STARTED_PER_POLL, "one poll's worth");
    // The next result waits for a run, like one that had to.
    assert_eq!(lio.try_run().unwrap(), 1);
    assert_eq!(turns.get(), 2 * STARTED_PER_POLL + 1);
    lio.cancel(&spawned);
    lio.try_run().unwrap();
    assert!(!lio.is_full());
  }

  #[test]
  fn a_model_done_at_its_first_poll_never_takes_a_slot() {
    let lio = Lio::new_with_backend(AtOnce, 1).unwrap();
    let seen = Rc::new(Cell::new(false));
    let (set, inside) = (seen.clone(), lio.clone());
    crate::api::nop().with_lio(&lio).when_done(move |_| {
      assert!(!inside.is_full(), "the only slot is free while it runs");
      set.set(true);
    });
    assert!(seen.get());
  }

  /// Submits a nop whose callback submits the next, `left` times.
  fn chain(lio: &Lio, left: &Rc<Cell<u32>>, depth: &Rc<Cell<(u32, u32)>>) {
    if left.get() == 0 {
      return;
    }
    left.set(left.get() - 1);
    let (lio_in, left_in, depth_in) =
      (lio.clone(), left.clone(), depth.clone());
    crate::api::nop().with_lio(lio).when_done(move |_| {
      let (now, deepest) = depth_in.get();
      depth_in.set((now + 1, deepest.max(now + 1)));
      chain(&lio_in, &left_in, &depth_in);
      let (now, deepest) = depth_in.get();
      depth_in.set((now - 1, deepest));
    });
  }

  #[test]
  fn callbacks_that_submit_from_callbacks_do_not_nest_without_end() {
    let lio = Lio::new_with_backend(AtOnce, 4).unwrap();
    let left = Rc::new(Cell::new(10_000));
    let depth = Rc::new(Cell::new((0, 0)));
    chain(&lio, &left, &depth);
    // Each run delivers at least one. A driver that stops delivering
    // fails the count below, where a loop until done would hang.
    for _ in 0..10_000 {
      lio.try_run().unwrap();
    }
    assert_eq!(left.get(), 0, "every link of the chain ran");
    let (now, deepest) = depth.get();
    assert_eq!(now, 0);
    assert!(deepest <= STARTED_PER_POLL + 1, "nested {deepest} deep");
    assert!(!lio.is_full());
  }

  /// Fills the driver from inside its own first poll, then runs a nop.
  struct Greedy {
    lio: Lio,
    grabbed: bool,
    nop: Nop,
  }

  impl OpModel for Greedy {
    type Item = ();

    fn poll(&mut self, cx: &mut Cx<'_>) -> Step<()> {
      if !self.grabbed {
        self.grabbed = true;
        crate::api::nop().with_lio(&self.lio).when_done(|_| {});
      }
      let _ = ready!(self.nop.poll(cx));
      Step::Done(())
    }
  }

  #[test]
  fn an_operation_started_with_no_slot_to_be_had_waits_for_its_model_s() {
    let (backend, lio) = lio(1);
    let done = Rc::new(Cell::new(false));
    let set = done.clone();
    let model = Greedy { lio: lio.clone(), grabbed: false, nop: Nop::new() };
    crate::api::run(model).with_lio(&lio).when_done(move |()| set.set(true));
    // The one slot holds what the model submitted on the side: its own
    // operation has not reached the backend.
    let [aside] = backend.pushed()[..] else { panic!("one op") };
    backend.complete(aside, 0);
    lio.try_run().unwrap();
    lio.try_run().unwrap();
    let [_, own] = backend.pushed()[..] else { panic!("two ops") };
    assert!(!done.get());
    backend.complete(own, 0);
    lio.try_run().unwrap();
    assert!(done.get());
    assert!(!lio.is_full());
  }

  #[test]
  fn a_future_done_as_it_was_spawned_leaves_no_waker_behind() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::Wake;

    /// Counts its wakes. Its reference count says who still holds it.
    struct Counted(AtomicUsize);
    impl Wake for Counted {
      fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
      }
    }

    let lio = Lio::new_with_backend(AtOnce, 1).unwrap();
    let quiet = Arc::new(Counted(AtomicUsize::new(0)));
    let waker = Waker::from(quiet.clone());
    // In a slot from the start, where the backend finishes it at once.
    let spawned = lio.spawn_awaited(Nop::new(), &waker);
    // SAFETY: spawned just above, for a model whose item is this.
    let item = unsafe { lio.take_awaited::<io::Result<()>>(&spawned, &waker) };
    assert!(matches!(item, Some(Ok(()))));
    assert!(!lio.is_full());
    drop(waker);
    drop(lio);
    assert_eq!(Arc::strong_count(&quiet), 1, "the slot kept a waker");
    assert_eq!(quiet.0.load(Ordering::Relaxed), 0, "nothing was waiting");
  }

  #[test]
  fn a_run_with_nothing_to_wait_for_does_not_block() {
    let (_, lio) = lio(1);
    assert_eq!(lio.run().unwrap(), 0);
  }

  #[test]
  fn a_run_with_nothing_to_wait_for_leaves_the_backend_alone() {
    let timeouts = Rc::new(RefCell::new(Vec::new()));
    let backend =
      Recording { inner: ScriptedBackend::new(), timeouts: timeouts.clone() };
    let lio = Lio::new_with_backend(backend, 1).unwrap();
    assert_eq!(lio.run().unwrap(), 0);
    assert_eq!(lio.try_run().unwrap(), 0);
    assert!(timeouts.borrow().is_empty(), "no wait was made");
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
  #[cfg_attr(not(panic = "unwind"), ignore = "catches a panic")]
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
    let queued = lio.spawn(crate::api::nop(), move |_| {
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
    let queued = lio.spawn(crate::api::nop(), |_| true);
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

  impl IoBackend for Lowering {
    fn init(&mut self, cap: usize) -> io::Result<()> {
      self.inner.init(cap)
    }
    fn start(
      &mut self,
      op: Queued<'_>,
      bump: &mut Bump,
      done: &mut Done<'_>,
    ) -> Started {
      let at = bump.alloc(0u64) as *mut u64 as usize;
      self.first_allocs.borrow_mut().push(at);
      self.inner.start(op, bump, done)
    }
    fn wait(
      &mut self,
      timeout: Option<Duration>,
      done: &mut Done<'_>,
    ) -> io::Result<()> {
      self.inner.wait(timeout, done)
    }

    fn resource_close(&self) -> crate::api::resource::ResourceClose {
      self.inner.resource_close()
    }

    fn cancel(&mut self, key: OpKey, done: &mut Done<'_>) {
      self.inner.cancel(key, done)
    }
  }

  #[test]
  fn every_operation_lowers_into_its_slot_s_reset_arena() {
    let backend = Lowering::default();
    let lio = Lio::new_with_backend(backend.clone(), 1).unwrap();
    let kernel = Kernel::new(backend.inner.clone());
    for _ in 0..2 {
      crate::api::nop().with_lio(&lio).when_done(|_| {});
      kernel.complete_all(0);
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
  #[cfg_attr(not(panic = "unwind"), ignore = "catches a panic")]
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

  /// Completes every op by the next wait, and records each wait's timeout.
  struct Recording {
    inner: ScriptedBackend,
    timeouts: Rc<RefCell<Vec<Option<Duration>>>>,
  }

  impl IoBackend for Recording {
    fn init(&mut self, cap: usize) -> io::Result<()> {
      self.inner.init(cap)
    }
    fn resource_close(&self) -> crate::api::resource::ResourceClose {
      self.inner.resource_close()
    }
    fn start(
      &mut self,
      op: Queued<'_>,
      bump: &mut Bump,
      done: &mut Done<'_>,
    ) -> Started {
      self.inner.start(op, bump, done)
    }
    fn wait(
      &mut self,
      timeout: Option<Duration>,
      done: &mut Done<'_>,
    ) -> io::Result<()> {
      self.timeouts.borrow_mut().push(timeout);
      self.inner.wait(timeout, done)
    }

    fn cancel(&mut self, key: OpKey, done: &mut Done<'_>) {
      self.inner.cancel(key, done)
    }
  }

  #[test]
  #[cfg_attr(not(panic = "unwind"), ignore = "catches a panic")]
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
  #[cfg_attr(not(panic = "unwind"), ignore = "catches a panic")]
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
  #[cfg_attr(not(panic = "unwind"), ignore = "catches a panic")]
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

  /// Nothing is waiting to be dropped, being dropped, or retired.
  fn assert_nothing_buried(lio: &Lio) {
    let inner = lio.inner.borrow();
    assert_eq!(
      (inner.dying.len(), inner.burying, inner.store.retired()),
      (0, 0, 0),
      "dying, burying, retired"
    );
  }

  /// Counts its drops.
  struct NoteDrop(Rc<Cell<usize>>);

  impl Drop for NoteDrop {
    fn drop(&mut self) {
      self.0.set(self.0.get() + 1);
    }
  }

  /// Starts a nop whose registration holds `held`, and takes the
  /// registration out of the store to wait in `dying`, as a run does with
  /// one it has to drop later. The nop stays with the backend.
  fn leave_to_bury<T: 'static>(backend: &Kernel, lio: &Lio, held: T) {
    crate::api::nop().with_lio(lio).when_done(move |_| {
      let _held = &held;
    });
    let Some(&id) = backend.pushed().last() else {
      panic!("the nop was not handed to the backend");
    };
    let mut inner = lio.inner.borrow_mut();
    let detached = inner.store.detach(id).unwrap();
    inner.dying.push(detached);
  }

  #[test]
  fn several_registrations_are_buried_together() {
    let (backend, lio) = lio(3);
    let dropped = Rc::new(Cell::new(0));
    for _ in 0..3 {
      leave_to_bury(&backend, &lio, NoteDrop(dropped.clone()));
    }
    assert!(lio.is_full(), "their slots are held until they are dropped");

    lio.bury_and_wake();
    assert_eq!(dropped.get(), 3);
    assert_nothing_buried(&lio);
    assert!(!lio.is_full());
  }

  #[test]
  #[cfg_attr(not(panic = "unwind"), ignore = "catches a panic")]
  fn a_panic_in_the_middle_of_a_batch_keeps_every_slot_accounted() {
    let (backend, lio) = lio(3);
    let dropped = Rc::new(Cell::new(0));
    // Buried last to first: one is dropped, the next panics, one is left.
    leave_to_bury(&backend, &lio, NoteDrop(dropped.clone()));
    leave_to_bury(&backend, &lio, Bomb);
    leave_to_bury(&backend, &lio, NoteDrop(dropped.clone()));

    let bury = std::panic::AssertUnwindSafe(|| lio.bury_and_wake());
    assert!(std::panic::catch_unwind(bury).is_err(), "the bomb went off");
    assert_eq!(dropped.get(), 1, "the one before the panic was dropped");
    {
      let inner = lio.inner.borrow();
      assert_eq!(inner.dying.len(), 1, "the one after it waits");
      assert_eq!(inner.burying, 0, "nothing is being dropped any more");
      assert_eq!(inner.store.retired(), 1, "two slots went back");
    }

    lio.bury_and_wake();
    assert_eq!(dropped.get(), 2);
    assert_nothing_buried(&lio);
    assert!(!lio.is_full());
  }

  #[test]
  fn the_retired_slot_check_adds_up_what_waits_and_what_is_being_dropped() {
    let (backend, lio) = lio(2);
    crate::api::nop().with_lio(&lio).when_done(|_| {});
    crate::api::nop().with_lio(&lio).when_done(|_| {});
    let (waiting, dropping) = (backend.pushed()[0], backend.pushed()[1]);
    // One registration waits in `dying`, and one is held as if a bury were
    // dropping it right now.
    let (waiting, dropping) = {
      let mut inner = lio.inner.borrow_mut();
      let waiting = inner.store.detach(waiting).unwrap();
      let dropping = inner.store.detach(dropping).unwrap();
      inner.dying.push(waiting);
      inner.burying += 1;
      Lio::assert_retired_slots_tracked(&inner);
      (inner.dying.pop().unwrap(), dropping)
    };
    // Put back as a bury would leave it. Dropped with the driver not
    // borrowed: they hold models.
    let slots = [waiting.slot, dropping.slot];
    drop((waiting.registration, dropping.registration));
    {
      let mut inner = lio.inner.borrow_mut();
      for slot in slots {
        inner.store.release(slot);
      }
      inner.burying -= 1;
      Lio::assert_retired_slots_tracked(&inner);
    }
    assert_nothing_buried(&lio);
  }

  /// Starts a nop and hands it out before it is done.
  struct HandsOut {
    nop: Option<Nop>,
    out: Rc<RefCell<Option<Nop>>>,
  }

  impl OpModel for HandsOut {
    type Item = ();

    fn poll(&mut self, cx: &mut Cx<'_>) -> Step<()> {
      if let Some(mut nop) = self.nop.take() {
        assert!(matches!(nop.poll(cx), Step::Pending));
        *self.out.borrow_mut() = Some(nop);
      }
      Step::Done(())
    }
  }

  /// Waits for the nop it finds in `from`.
  struct PicksUp {
    from: Rc<RefCell<Option<Nop>>>,
  }

  impl OpModel for PicksUp {
    type Item = ();

    fn poll(&mut self, cx: &mut Cx<'_>) -> Step<()> {
      let mut from = self.from.borrow_mut();
      let Some(nop) = from.as_mut() else {
        panic!("no operation was handed out");
      };
      let _ = ready!(nop.poll(cx));
      Step::Done(())
    }
  }

  #[test]
  fn an_operation_is_never_taken_from_another_driver() {
    let (first_backend, first) = lio(2);
    let (second_backend, second) = lio(2);
    // An operation of the first driver, in its first slot.
    let out = Rc::new(RefCell::new(None));
    first
      .spawn(HandsOut { nop: Some(Nop::new()), out: out.clone() }, |()| true);
    assert_eq!(first_backend.pushed().len(), 1);

    // The second driver has one of its own in the same slot, with the same
    // id, and a model there is handed the first driver's.
    let own = Rc::new(Cell::new(0));
    let delivered = own.clone();
    crate::api::nop()
      .with_lio(&second)
      .when_done(move |_| delivered.set(delivered.get() + 1));
    let foreign = Rc::new(Cell::new(0));
    second.spawn(PicksUp { from: out.clone() }, counting(&foreign));

    second_backend.complete_all(0);
    second.try_run().unwrap();
    assert_eq!(own.get(), 1, "its own operation went to its own consumer");
    assert_eq!(foreign.get(), 0, "the other driver's operation is not here");

    // Dropped on the driver it belongs to, which is asked to stop it.
    drop(out.borrow_mut().take());
    assert_eq!(first_backend.cancelled().len(), 1);
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

  use crate::api::ops::{Nop, Sleep};
  use crate::model::{Cx, Join, OpModel, Step};
  use crate::ready;

  /// Counts the models that finished.
  fn counting(count: &Rc<Cell<usize>>) -> impl FnMut(()) -> bool + 'static {
    let count = count.clone();
    move |()| {
      count.set(count.get() + 1);
      true
    }
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
  fn models_that_finish_at_once_hand_their_slots_on_in_the_same_run() {
    let (backend, lio) = lio(2);
    crate::api::nop().with_lio(&lio).when_done(|_| {});
    crate::api::nop().with_lio(&lio).when_done(|_| {});
    let done = Rc::new(Cell::new(0));
    for _ in 0..3 {
      let queued = lio.spawn(Now, counting(&done));
      assert!(matches!(queued, Spawned::Queued(_)));
    }

    backend.complete_all(0);
    lio.try_run().unwrap();
    // The first two take the nops' slots, and the third takes one of
    // theirs.
    assert_eq!(done.get(), 3);
    assert!(!lio.is_full());
  }

  #[test]
  fn two_sleeps_of_one_model_each_get_a_slot() {
    let (_, lio) = lio(2);
    crate::time::pause(&lio);
    let done = Rc::new(Cell::new(0));
    let count = done.clone();
    let both = Join::new(
      Sleep::new(Duration::from_millis(1)),
      Sleep::new(Duration::from_millis(2)),
    );
    lio.spawn(both, move |(first, second)| {
      assert!(first.is_ok() && second.is_ok());
      count.set(count.get() + 1);
      true
    });
    assert!(lio.is_full(), "nothing holds a sleep's lease, and still");

    lio.advance_time_by_ticks(5);
    lio.try_run().unwrap();
    assert_eq!(done.get(), 1);
    assert!(!lio.is_full());
  }

  /// Runs two nops, one after the other.
  struct NopTwice {
    first: Option<Nop>,
    second: Nop,
  }

  impl OpModel for NopTwice {
    type Item = ();

    fn poll(&mut self, cx: &mut Cx<'_>) -> Step<()> {
      if let Some(first) = self.first.as_mut() {
        let _ = ready!(first.poll(cx));
        self.first = None;
      }
      let _ = ready!(self.second.poll(cx));
      Step::Done(())
    }
  }

  /// Begins every operation and never reports it: its lease stays held.
  ///
  /// With the completion [`run_hoarding`] makes up for each, this is a
  /// backend that reports an operation while something still uses its
  /// memory. The table does not let a real backend do that: the driver
  /// must not reuse that memory all the same.
  #[derive(Default)]
  struct Hoarding {
    pushed: Rc<RefCell<Vec<u64>>>,
    pending: Rc<RefCell<Vec<u64>>>,
  }

  impl IoBackend for Hoarding {
    fn init(&mut self, _: usize) -> io::Result<()> {
      Ok(())
    }
    fn start(
      &mut self,
      op: Queued<'_>,
      _: &mut Bump,
      _: &mut Done<'_>,
    ) -> Started {
      let id = op.id();
      self.pending.borrow_mut().push(id);
      self.pushed.borrow_mut().push(id);
      // Begun and never reported.
      op.begin().0
    }
    fn wait(
      &mut self,
      _: Option<Duration>,
      _: &mut Done<'_>,
    ) -> io::Result<()> {
      Ok(())
    }

    fn resource_close(&self) -> crate::api::resource::ResourceClose {
      crate::api::resource::ResourceClose::native()
    }

    // Its operations finish on their own: there is nothing to stop.
    fn cancel(&mut self, _: OpKey, _: &mut Done<'_>) {}
  }

  /// Runs the driver with a made-up completion for everything `Hoarding`
  /// was given since the last time.
  fn run_hoarding(lio: &Lio, pending: &Rc<RefCell<Vec<u64>>>) {
    // Hand over what is queued first, so that it is pending.
    for id in pending.borrow_mut().drain(..) {
      lio.inner.borrow_mut().io.forge(id, 0);
    }
    lio.try_run().unwrap();
  }

  #[test]
  #[cfg_attr(miri, ignore = "leaks the registrations on purpose")]
  fn a_slot_whose_lease_is_still_held_takes_no_further_operation() {
    let backend = Hoarding::default();
    let (pushed, pending) = (backend.pushed.clone(), backend.pending.clone());
    let lio = Lio::new_with_backend(backend, 2).unwrap();
    let done = Rc::new(Cell::new(0));
    let model = NopTwice { first: Some(Nop::new()), second: Nop::new() };
    lio.spawn(model, counting(&done));

    run_hoarding(&lio, &pending);
    run_hoarding(&lio, &pending);
    let [first, second] = pushed.borrow()[..] else { panic!("two ops") };
    assert_ne!(first, second, "the backend may still use the first's memory");
    assert_eq!(done.get(), 1);
    assert_eq!(lio.inner.borrow().leaked, 2);
  }

  /// Starts a nop, gives up on it, and runs another.
  struct Fickle {
    given_up: Option<Nop>,
    kept: Nop,
  }

  impl OpModel for Fickle {
    type Item = ();

    fn poll(&mut self, cx: &mut Cx<'_>) -> Step<()> {
      if let Some(mut nop) = self.given_up.take() {
        assert!(matches!(nop.poll(cx), Step::Pending));
      }
      let _ = ready!(self.kept.poll(cx));
      Step::Done(())
    }
  }

  #[test]
  fn a_model_outlives_an_operation_it_gave_up_on() {
    let (backend, lio) = lio(2);
    let done = Rc::new(Cell::new(0));
    let model = Fickle { given_up: Some(Nop::new()), kept: Nop::new() };
    lio.spawn(model, counting(&done));
    // The backend has the one it gave up on, and is asked to stop it.
    let [given_up, kept] = backend.pushed()[..] else { panic!("two ops") };
    assert_eq!(backend.cancelled(), [given_up]);

    // It finishes in the model's own slot, which the model still needs.
    backend.complete(given_up, -1);
    assert_eq!(lio.try_run().unwrap(), 1);
    assert_eq!(done.get(), 0);

    backend.complete(kept, 0);
    lio.try_run().unwrap();
    assert_eq!(done.get(), 1);
    assert!(!lio.is_full());
  }

  impl Cx<'_> {
    /// A nop whose result cannot be read.
    fn failing(&mut self) -> crate::model::Ticket<()> {
      self.submit(
        (),
        |_| crate::api::op::Action::nop(),
        |_, _| panic!("no result for you"),
      )
    }
  }

  leaf!([] struct Failing() -> () = cx.failing);

  /// A blocking run after one that unwound out of `Failing`'s result:
  /// the timeout it gave the backend.
  fn timeout_after_a_failed_run(queue: impl FnOnce(&Lio)) -> Option<Duration> {
    let timeouts = Rc::new(RefCell::new(Vec::new()));
    let backend =
      Recording { inner: ScriptedBackend::auto(0), timeouts: timeouts.clone() };
    let lio = Lio::new_with_backend(backend, 4).unwrap();
    crate::time::pause(&lio);
    lio.spawn(Failing::new(), |()| true);
    queue(&lio);
    lio.advance_time_by_ticks(5);
    let first = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
      lio.try_run().unwrap();
    }));
    assert!(first.is_err());
    assert!(lio.inner.borrow().woken.is_empty(), "no model is left to poll");
    assert_eq!(
      lio.inner.borrow().time.wait_timeout(None),
      None,
      "no timer is due that would keep the run from blocking either"
    );

    assert_eq!(lio.run().unwrap(), 1, "what was queued is routed");
    let last = timeouts.borrow().last().copied();
    last.flatten()
  }

  #[test]
  #[cfg_attr(not(panic = "unwind"), ignore = "catches a panic")]
  fn a_queued_completion_alone_keeps_a_run_from_blocking() {
    let timeout = timeout_after_a_failed_run(|lio| {
      crate::api::nop().with_lio(lio).when_done(|_| {});
    });
    assert_eq!(timeout, Some(Duration::ZERO));
  }

  #[test]
  #[cfg_attr(not(panic = "unwind"), ignore = "catches a panic")]
  fn a_fired_timer_alone_keeps_a_run_from_blocking() {
    let timeout = timeout_after_a_failed_run(|lio| {
      crate::api::sleep(Duration::from_millis(1))
        .with_lio(lio)
        .when_done(|_| {});
    });
    assert_eq!(timeout, Some(Duration::ZERO));
  }

  #[test]
  #[cfg_attr(not(panic = "unwind"), ignore = "catches a panic")]
  fn a_slot_whose_result_could_not_be_read_goes_back_on_the_next_run() {
    let (backend, lio) = lio(2);
    // The nop takes the model's slot, and the failing one a slot of its own.
    lio.spawn(Join::new(Nop::new(), Failing::new()), |_| true);
    backend.complete_all(0);
    let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
      lio.try_run().unwrap();
    }));
    assert!(run.is_err());
    assert_eq!(lio.inner.borrow().dying.len(), 1);
    assert!(lio.is_full());

    // Nothing else is left for this run to do.
    assert_eq!(lio.try_run().unwrap(), 0);
    assert!(lio.inner.borrow().dying.is_empty());
    assert!(!lio.is_full());
  }

  #[test]
  fn a_cancelled_model_is_dropped_by_the_run_that_reaches_it_in_the_queue() {
    let (backend, lio) = lio(1);
    crate::api::nop().with_lio(&lio).when_done(|_| {});
    let dropped = Rc::new(Cell::new(false));
    let flag = DropFlag(dropped.clone());
    let queued = lio.spawn(Nop::new(), move |_| {
      let _flag = &flag;
      true
    });
    lio.cancel(&queued);
    assert!(!dropped.get(), "it holds user values: not dropped under a borrow");

    backend.complete_all(0);
    lio.try_run().unwrap();
    assert!(dropped.get());
  }
}
