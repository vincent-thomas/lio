use crate::{
  api::op::{Action, Completion},
  backend::{IoBackend, OpCompleted, store::OpStore},
  registration::Registration,
  time::TimeManager,
};

use std::{
  cell::{RefCell, RefMut},
  collections::VecDeque,
  io,
  rc::Rc,
  task::Waker,
  time::{Duration, Instant},
};

use bumpalo::Bump;

/// Result code returned to userspace when a sleep timer fires.
/// Always `0` — the driver normalizes timer expiry before routing
/// to models, so models never need to interpret OS-specific errno values.
const SLEEP_RESULT: isize = 0;

thread_local! {
  static GLOBAL_LIO: RefCell<Option<Lio>> = const { RefCell::new(None) };
}

/// Guard returned by [`install_global`].
///
/// Dropping the guard uninstalls the thread-local global [`Lio`].
#[must_use = "Dropping the guard immediately uninstalls the global Lio"]
pub struct GlobalLioGuard {
  installed: bool,
}

impl Drop for GlobalLioGuard {
  fn drop(&mut self) {
    if self.installed {
      let _ = uninstall_global();
    }
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
    *global = Some(lio);
  });
  GlobalLioGuard { installed: true }
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
  // Field order is drop order: the backend must shut down (finishing or
  // cancelling in-flight work) before `store` frees the buffers that work
  // still points into.
  io: Box<dyn IoBackend>,
  store: OpStore,
  /// The backend's close for resources its completions carry.
  resource_close: crate::api::resource::CloseFn,
  time: TimeManager,
  // Reused scratch vectors for `run_inner`.
  completed: Vec<OpCompleted>,
  expired_timers: Vec<u64>,
  /// Completions and timers a panicking run did not get to; the next run
  /// processes them.
  carry_completed: Vec<OpCompleted>,
  carry_timers: Vec<u64>,
  /// Submissions made while every slot was taken, scheduled in order as
  /// slots free up. Only allocates when the driver is over capacity.
  overflow: VecDeque<PendingSubmission>,
  /// Futures waiting for a free slot.
  capacity_waiters: Vec<Waker>,
  profile: Option<LioProfile>,
}

/// A registration to build once a slot is free.
type PendingSubmission = Box<dyn FnOnce(&mut Bump) -> Registration>;

#[derive(Debug, Default)]
struct LioProfile {
  run_inner_calls: usize,
  completions_returned: usize,
  stale_completions: usize,
  timer_expirations: usize,
  dispatch_count: usize,
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
  dispatch_time: Duration,
  remove_time: Duration,
}

impl LioProfile {
  fn merge(&mut self, run: &LioProfile) {
    self.run_inner_calls += 1;
    self.completions_returned += run.completions_returned;
    self.stale_completions += run.stale_completions;
    self.timer_expirations += run.timer_expirations;
    self.dispatch_count += run.dispatch_count;
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
    self.dispatch_time += run.dispatch_time;
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
    let Some(profile) = &self.profile else {
      return;
    };
    eprintln!(
      "\nlio-profile run_inner_calls={} completions_returned={} stale_completions={} timer_expirations={} dispatch_count={} remove_count={} flush_ms={:.3} next_deadline_ms={:.3} wait_ms={:.3} completion_loop_ms={:.3} completion_store_lookup_ms={:.3} completion_on_completion_ms={:.3} completion_is_finished_ms={:.3} timer_poll_ms={:.3} timer_loop_ms={:.3} dispatch_ms={:.3} remove_ms={:.3}",
      profile.run_inner_calls,
      profile.completions_returned,
      profile.stale_completions,
      profile.timer_expirations,
      profile.dispatch_count,
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
      profile.dispatch_time.as_secs_f64() * 1000.0,
      profile.remove_time.as_secs_f64() * 1000.0,
    );
  }
}

#[derive(Clone)]
pub struct Lio {
  inner: Rc<RefCell<LioInner>>,
}

impl Lio {
  pub(crate) fn pause_time(&self) {
    self.inner.borrow_mut().time.pause();
  }

  pub(crate) fn resume_time(&self) {
    self.inner.borrow_mut().time.resume();
  }

  pub(crate) fn advance_time_by_ticks(&self, ticks: u64) {
    self.inner.borrow_mut().time.advance_by_ticks(ticks);
  }

  fn dispatch_action(inner: &mut LioInner, id: u64, action: Action) {
    let (store, io, time) = (&mut inner.store, &mut inner.io, &mut inner.time);
    match action {
      Action::Io(op) => {
        let step_bump = store
          .step_bump_mut(id)
          .expect("dispatching action for unknown registration");
        step_bump.reset();
        io.push(id, op, step_bump);
      }
      Action::Sleep(duration) => time.schedule(id, duration),
    }
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
  pub fn new_with_backend<D>(mut backend: D, cap: usize) -> io::Result<Self>
  where
    D: IoBackend + 'static,
  {
    backend.init(cap)?;

    let inner = LioInner {
      resource_close: backend.resource_close(),
      io: Box::new(backend),
      store: OpStore::with_capacity(cap),
      time: TimeManager::with_capacity(cap),
      completed: Vec::with_capacity(cap),
      expired_timers: Vec::with_capacity(cap),
      carry_completed: Vec::new(),
      carry_timers: Vec::new(),
      overflow: VecDeque::new(),
      capacity_waiters: Vec::new(),
      profile: std::env::var_os("LIO_PROFILE").map(|_| LioProfile::default()),
    };
    Ok(Self { inner: Rc::new(RefCell::new(inner)) })
  }

  /// Schedule a registration built inside the store slot's persistent bump arena.
  #[inline]
  pub(crate) fn schedule_with<F>(&self, init: F) -> io::Result<u64>
  where
    F: FnOnce(&mut Bump) -> Registration,
  {
    let mut inner = self.inner.borrow_mut();
    // Queued submissions go first.
    if !inner.overflow.is_empty() || inner.store.is_full() {
      return Err(Self::full_error());
    }
    Self::schedule_now(&mut inner, init)
  }

  /// Schedules `init` now, or queues it until a slot frees up.
  #[inline]
  pub(crate) fn schedule_or_queue(
    &self,
    init: impl FnOnce(&mut Bump) -> Registration + 'static,
  ) {
    let mut inner = self.inner.borrow_mut();
    if inner.overflow.is_empty() && !inner.store.is_full() {
      // An op without a first action is dropped; only `schedule_with` can
      // report that.
      let _ = Self::schedule_now(&mut inner, init);
    } else {
      Self::enqueue(&mut inner, init);
    }
  }

  /// Queues a submission until a slot frees up. Kept out of line: it only
  /// runs when the driver is over capacity.
  #[cold]
  #[inline(never)]
  fn enqueue(
    inner: &mut LioInner,
    init: impl FnOnce(&mut Bump) -> Registration + 'static,
  ) {
    inner.overflow.push_back(Box::new(init));
  }

  /// Inserts a registration and dispatches its first action. The store must
  /// have a free slot.
  #[inline]
  fn schedule_now(
    inner: &mut LioInner,
    init: impl FnOnce(&mut Bump) -> Registration,
  ) -> io::Result<u64> {
    let (store, io, time) = (&mut inner.store, &mut inner.io, &mut inner.time);
    let (id, action, step_bump) = store.insert_with_action(init);
    let Some(action) = action else {
      store.remove(id);
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "OpModel returned no action on first call",
      ));
    };
    match action {
      Action::Io(op) => io.push(id, op, step_bump),
      Action::Sleep(duration) => time.schedule(id, duration),
    }
    Ok(id)
  }

  fn full_error() -> io::Error {
    io::Error::new(io::ErrorKind::WouldBlock, "lio is at capacity")
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
      && let Some(init) = inner.overflow.pop_front()
    {
      // An op without a first action is dropped, as `schedule_with` reports
      // for direct submissions.
      let _ = Self::schedule_now(inner, init);
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
    let (processed, waiters) = if inner.profile.is_some() {
      self.run_with::<true>(inner, timeout)?
    } else {
      self.run_with::<false>(inner, timeout)?
    };
    for waker in waiters {
      waker.wake();
    }
    Ok(processed)
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
  ) -> io::Result<(usize, Vec<Waker>)> {
    let mut stats = LioProfile::default();
    let inner_ref = &mut *inner;

    timed!(PROFILE, stats, flush_time, inner_ref.io.flush())?;

    // Compute effective timeout: min of user timeout and next timer deadline.
    // Avoid touching the clock on the common pure-I/O path.
    let has_timers = !inner_ref.time.is_empty();
    let next_deadline = if has_timers {
      timed!(PROFILE, stats, next_deadline_time, inner_ref.time.next_deadline())
    } else {
      None
    };
    let effective_timeout = match (timeout, next_deadline) {
      (Some(user), Some(timer)) => Some(user.min(timer)),
      (Some(user), None) => Some(user),
      (None, Some(timer)) => Some(timer),
      (None, None) => None,
    };

    inner_ref.completed.clear();
    let (io, completed) = (&mut inner_ref.io, &mut inner_ref.completed);
    timed!(PROFILE, stats, wait_time, io.wait(effective_timeout, completed))?;

    // The lists leave `inner` while being routed: a nested run (from user
    // code) gets fresh ones instead of clobbering them.
    let mut completed = std::mem::take(&mut inner_ref.completed);
    if !inner_ref.carry_completed.is_empty() {
      completed.append(&mut inner_ref.carry_completed);
    }
    let carried_timers = !inner_ref.carry_timers.is_empty();
    let mut timers = if has_timers || carried_timers {
      let mut timers = std::mem::take(&mut inner_ref.expired_timers);
      timers.clear();
      timers.append(&mut inner_ref.carry_timers);
      timers
    } else {
      Vec::new()
    };
    // `has_timers` also includes fired timers waiting to be drained, so an
    // empty clock is safe to skip entirely.
    if has_timers {
      timed!(
        PROFILE,
        stats,
        timer_poll_time,
        timers.extend(inner_ref.time.poll_expired())
      );
      stats.timer_expirations += timers.len();
    }
    let resource_close = inner_ref.resource_close;

    let loop_started = PROFILE.then(Instant::now);
    let mut batch = Batch {
      lio: self,
      completed,
      timers,
      next_io: 0,
      next_timer: 0,
      current: None,
      finished: false,
    };
    let mut processed = 0;
    while let Some(&c) = batch.completed.get(batch.next_io) {
      batch.next_io += 1;
      let completion =
        Completion::new(c.result()).with_resource_close(resource_close);
      let routed;
      (inner, routed) = self.route::<PROFILE>(
        inner,
        &mut batch,
        c.registration_id(),
        completion,
        false,
        &mut stats,
      );
      if routed {
        processed += 1;
      } else {
        stats.stale_completions += 1;
      }
    }
    while let Some(&id) = batch.timers.get(batch.next_timer) {
      batch.next_timer += 1;
      let completion = Completion::with_flags(
        SLEEP_RESULT,
        crate::api::op::CompletionFlags::TIMER,
      );
      (inner, _) = self
        .route::<PROFILE>(inner, &mut batch, id, completion, true, &mut stats);
      processed += 1;
    }
    if let Some(started) = loop_started {
      stats.completion_loop_time += started.elapsed();
    }

    batch.finish(&mut inner);
    if PROFILE && let Some(profile) = inner.profile.as_mut() {
      stats.completions_returned = processed;
      profile.merge(&stats);
    }
    Ok((processed, Self::release_capacity(&mut inner)))
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
      inner.store.get_mut(id).map(|reg| reg.begin_completion())
    );
    let Some((handler, mut waker)) = begun else {
      return (inner, false);
    };

    drop(inner);
    batch.current = Some(id);
    let result = timed!(PROFILE, stats, completion_on_completion_time, {
      // SAFETY: the registration was alive above, and stays alive until
      // removed below: dropping its consumer only marks it cancelled. For a
      // cancelled operation the op model still sees the result, so what it
      // produced (an accepted socket, say) is released, but nobody is woken.
      unsafe { handler.complete(completion, &mut waker) }
    });
    batch.current = None;
    let mut inner = self.inner.borrow_mut();

    let inner_ref = &mut *inner;
    if let Some(reg) = inner_ref.store.get_mut(id) {
      // A cancelled operation takes no further step; its consumer may also
      // have been dropped by the wake-up just now.
      let next_action =
        if reg.is_cancelled() { None } else { result.next_action };
      match next_action {
        Some(action) => {
          if let Some(waker) = waker {
            reg.restore_waker(waker);
          }
          timed!(
            PROFILE,
            stats,
            dispatch_time,
            Self::dispatch_action(inner_ref, id, action)
          );
          stats.dispatch_count += 1;
        }
        None => {
          timed!(PROFILE, stats, remove_time, inner_ref.store.remove_known(id));
          stats.remove_count += 1;
        }
      }
    }
    (inner, true)
  }

  // NOTE: check_done and check_stream_done are no longer needed with the channel-based
  // architecture. Results are now sent through mpsc channels directly to futures/streams.

  pub(crate) fn set_waker(&self, id: u64, waker: Waker) {
    let mut inner = self.inner.borrow_mut();
    if let Some(entry) = inner.store.get_mut(id) {
      entry.set_waker(waker);
    }
  }

  /// Cancels an operation whose consumer (a future or stream) was dropped.
  ///
  /// A pending timer is simply removed. An I/O operation may still be in the
  /// backend, using memory owned by its registration, so the registration
  /// stays until the operation's completion arrives; the backend is asked to
  /// finish it early, and the completion is discarded.
  pub(crate) fn cancel_stream(&self, id: u64) {
    let waiters = {
      let mut inner = self.inner.borrow_mut();
      let inner = &mut *inner;
      let Some(reg) = inner.store.get_mut(id) else {
        return;
      };
      if inner.time.remove(id) {
        inner.store.remove_known(id);
      } else {
        reg.cancel();
        inner.io.cancel(id);
        return;
      }
      Self::release_capacity(inner)
    };
    for waker in waiters {
      waker.wake();
    }
  }
}

/// The completions and expired timers of one run, consumed in order.
///
/// `finish` hands the lists back for reuse. If a handler panics instead,
/// dropping the batch removes that handler's registration (its state is
/// unknown) and carries the entries it did not reach over to the next run, so
/// their operations still complete.
struct Batch<'a> {
  lio: &'a Lio,
  completed: Vec<OpCompleted>,
  timers: Vec<u64>,
  next_io: usize,
  next_timer: usize,
  /// The registration whose handler is running.
  current: Option<u64>,
  finished: bool,
}

impl Batch<'_> {
  fn finish(&mut self, inner: &mut LioInner) {
    self.finished = true;
    self.completed.clear();
    self.timers.clear();
    // A nested run may have put back lists of its own; keep the larger.
    if inner.completed.capacity() < self.completed.capacity() {
      inner.completed = std::mem::take(&mut self.completed);
    }
    if inner.expired_timers.capacity() < self.timers.capacity() {
      inner.expired_timers = std::mem::take(&mut self.timers);
    }
  }
}

impl Drop for Batch<'_> {
  fn drop(&mut self) {
    if self.finished {
      return;
    }
    let Ok(mut inner) = self.lio.inner.try_borrow_mut() else {
      return;
    };
    if let Some(id) = self.current.take() {
      inner.store.remove(id);
    }
    inner.carry_completed.extend_from_slice(&self.completed[self.next_io..]);
    inner.carry_timers.extend_from_slice(&self.timers[self.next_timer..]);
  }
}
