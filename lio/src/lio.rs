use crate::{
  api::op::{Action, Completion},
  backend::{
    IoBackend, OpCompleted,
    store::{Detached, OpStore},
  },
  registration::Registration,
  time::TimeManager,
};

use std::{
  cell::{RefCell, RefMut},
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
  overflow: VecDeque<PendingSubmission>,
  /// Futures waiting for a free slot.
  capacity_waiters: Vec<Waker>,
  /// Tasks that polled an operation after it was removed but before its
  /// registration (and with it the stream's sender) was dropped. Woken once
  /// it is, so their next poll sees the stream end.
  orphaned_wakers: Vec<Waker>,
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
  fn count_timer_expirations(&mut self, before: usize, after: usize) {
    self.timer_expirations += after - before;
  }

  fn count_stale_completion(&mut self) {
    self.stale_completions += 1;
  }

  fn count_dispatch(&mut self) {
    self.dispatch_count += 1;
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

  /// Submits the registration's next action while the model is borrowed.
  ///
  /// Returns whether the model produced an action. The buffers live in the
  /// model, so [`InFlight::from_model`](crate::backend::InFlight::from_model)
  /// can name them only inside this borrow. The registration stays in the
  /// store, unmoved, until the completion is routed or the backend drains.
  fn submit_registration(inner: &mut LioInner, id: u64) -> bool {
    let LioInner { io, store, time, .. } = inner;
    let Some(ran) = store.with_slot(id, |reg, step_bump| {
      let keep = reg.lease();
      reg.with_action(&mut |action| match action {
        Action::Io(op) => {
          step_bump.reset();
          let flight = crate::backend::InFlight::from_model(op, keep.clone());
          let mut loan =
            crate::backend::loan::PushLoan::new(id, flight.as_op());
          io.push(id, flight.keep(), &mut loan, step_bump);
        }
        Action::Sleep(duration) => time.schedule(id, duration),
      })
    }) else {
      unreachable!("submitting registration {id}, which is not in the store");
    };
    ran
  }

  fn dispatch_action(inner: &mut LioInner, id: u64) {
    Self::submit_registration(inner, id);
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
      orphaned_wakers: Vec::new(),
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
    let scheduled = Self::schedule_now(&mut inner, init);
    drop(inner);
    if scheduled.is_err() {
      self.bury_and_wake();
    }
    scheduled
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
      let scheduled = Self::schedule_now(&mut inner, init);
      drop(inner);
      if scheduled.is_err() {
        self.bury_and_wake();
      }
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
    assert!(
      !inner.overflow.is_empty() || inner.store.is_full(),
      "driver: queued a submission while a slot is free"
    );
    inner.overflow.push_back(Box::new(init));
  }

  /// Inserts a registration and dispatches its first action. The store must
  /// have a free slot.
  #[inline]
  fn schedule_now(
    inner: &mut LioInner,
    init: impl FnOnce(&mut Bump) -> Registration,
  ) -> io::Result<u64> {
    let LioInner { io, store, time, .. } = inner;
    let Some(_id) =
      store.insert_submitting(init, |id, reg, step_bump, insert| {
        let keep = reg.lease();
        reg.with_action(&mut |action| {
          // After this, a panic must not free the model: `push` may have
          // copied its addresses. A panic inside `action` happens first and
          // still frees the slot.
          insert.keep_on_panic();
          match action {
            Action::Io(op) => {
              step_bump.reset();
              let flight =
                crate::backend::InFlight::from_model(op, keep.clone());
              let mut loan =
                crate::backend::loan::PushLoan::new(id, flight.as_op());
              io.push(id, flight.keep(), &mut loan, step_bump);
            }
            Action::Sleep(duration) => time.schedule(id, duration),
          }
        });
      })
    else {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "OpModel returned no action on first call",
      ));
    };
    Ok(_id)
  }

  /// Removes registration `id`, to be dropped by
  /// [`bury_and_wake`](Self::bury_and_wake) once the driver is not borrowed.
  #[inline]
  fn retire(inner: &mut LioInner, id: u64) {
    assert!(
      !inner.time.contains(id),
      "driver: removed registration {id} with its timer still scheduled"
    );
    // One copy, straight into `dying`. A held lease then leaves that list:
    // the registration is leaked and the slot stays reserved, so the buffers
    // the backend still uses stay valid.
    inner.store.detach_known_into(id, &mut inner.dying);
    let held = inner
      .dying
      .last()
      .is_some_and(|detached| detached.registration.lease_held());
    if held {
      let Some(detached) = inner.dying.pop() else {
        unreachable!(
          "driver: retire moved registration {id} and dying is empty"
        );
      };
      inner.leaked += 1;
      std::mem::forget(detached);
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
    let ran = if inner.profile.is_some() {
      self.run_with::<true>(inner, timeout)
    } else {
      self.run_with::<false>(inner, timeout)
    };
    // Also after an error: what this run removed is dropped either way.
    self.bury_and_wake();
    let (processed, waiters) = ran?;
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
      let mut inner = self.inner.borrow_mut();
      if inner.dying.is_empty() {
        return;
      }
      if inner.dying.len() == 1 {
        let Some(Detached { mut registration, slot }) = inner.dying.pop()
        else {
          unreachable!(
            "driver: dying reported one registration and did not have one"
          );
        };
        inner.burying += 1;
        drop(inner);
        // A task may have polled after the final item was sent, and found
        // the channel empty but still open. Wake it once the registration,
        // which holds the sender, is gone.
        let waker = registration.take_waker();
        {
          // Released even if the drop panics.
          let _release = ReleaseSlot { lio: self, slot };
          drop(registration);
        }
        if let Some(waker) = waker {
          waker.wake();
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
        let mut wakers = Vec::new();
        // Up to 256 slots, one release borrow after every drop is cheaper
        // than borrowing per registration. Past that, a second pass walks
        // slots that have left the cache, so release every 64 while the
        // registrations just dropped are still hot.
        let release_every =
          if batch.pending.len() > 256 { 64 } else { usize::MAX };
        while let Some(Detached { mut registration, slot }) =
          batch.pending.pop()
        {
          let waker = registration.take_waker();
          batch.dropping = Some(slot);
          drop(registration);
          batch.dropping = None;
          batch.slots.push(slot);
          if batch.slots.len() == release_every {
            self.release_buried_slots(&mut batch.slots);
          }
          if let Some(waker) = waker {
            wakers.push(waker);
          }
        }
        self.release_buried_slots(&mut batch.slots);
        let (waiters, orphaned) = {
          let mut inner = self.inner.borrow_mut();
          // Hand the buffers back. A nested bury may already have retired
          // something into the fresh `dying` queue; keep those.
          let spare_dying = std::mem::take(&mut batch.pending);
          if inner.dying.is_empty() {
            inner.dying = spare_dying;
          }
          inner.released_slots = std::mem::take(&mut batch.slots);
          let waiters = Self::release_capacity(&mut inner);
          (waiters, std::mem::take(&mut inner.orphaned_wakers))
        };
        // Slots are released and the buffers are back in the driver.
        // Skipping `Drop` avoids that out-of-line call on the path that
        // already finished.
        std::mem::forget(batch);
        for waker in wakers.into_iter().chain(waiters).chain(orphaned) {
          waker.wake();
        }
        continue;
      }

      // Scheduling queued submissions may remove ops without a first action,
      // so go around again until nothing is left to bury.
      let (waiters, orphaned) = {
        let mut inner = self.inner.borrow_mut();
        let waiters = Self::release_capacity(&mut inner);
        (waiters, std::mem::take(&mut inner.orphaned_wakers))
      };
      for waker in waiters.into_iter().chain(orphaned) {
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
  ) -> io::Result<(usize, Vec<Waker>)> {
    let mut stats = LioProfile::default();
    let inner_ref = &mut *inner;

    timed!(PROFILE, stats, flush_time, {
      let (io, store) = (&mut inner_ref.io, &mut inner_ref.store);
      io.flush(store)
    })?;

    // Work already queued is ready now; don't block on the backend first.
    let queued =
      !inner_ref.ready_io.is_empty() || !inner_ref.ready_timers.is_empty();
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
    let waited = timed!(PROFILE, stats, wait_time, {
      let (io, store, completed) =
        (&mut inner_ref.io, &mut inner_ref.store, &mut inner_ref.completed);
      io.wait(effective_timeout, completed, store)
    });
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
      // An earlier handler may have cancelled a sleep whose timer already
      // fired. An I/O registration, though, lives until its completion
      // arrives: one without it was completed twice, or never pushed.
      assert!(
        timer,
        "driver: backend completed operation {id}, which is not in flight"
      );
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
    let continues = {
      let Some(reg) = inner_ref.store.get_mut(id) else {
        panic!("driver: registration {id} was removed while its handler ran");
      };
      // A cancelled operation takes no further step; its consumer may also
      // have been dropped by the wake-up just now.
      let continues = !result.is_done() && !reg.is_cancelled();
      if continues && let Some(waker) = waker {
        reg.restore_waker(waker);
      }
      continues
    };
    if continues {
      timed!(
        PROFILE,
        stats,
        dispatch_time,
        Self::dispatch_action(inner_ref, id)
      );
      stats.count_dispatch();
    } else {
      // Dropped by `bury_and_wake` once the driver is not borrowed.
      timed!(PROFILE, stats, remove_time, Self::retire(inner_ref, id));
      stats.count_remove();
    }
    (inner, true)
  }

  /// Records `waker` on a waker registration. Polled streams do not use this:
  /// their waker lives in the stream's slot. Tests call it around bury.
  #[cfg(test)]
  pub(crate) fn set_waker(&self, id: u64, waker: Waker) {
    let mut inner = self.inner.borrow_mut();
    if let Some(entry) = inner.store.get_mut(id) {
      entry.set_waker(waker);
    } else if !inner.dying.is_empty() || inner.burying > 0 {
      // Removed, but perhaps not dropped yet: the task's channel may still
      // be open, so wake it once it is not.
      inner.orphaned_wakers.push(waker);
    } else {
      drop(inner);
      waker.wake();
    }
  }

  /// Cancels an operation whose consumer (a future or stream) was dropped.
  ///
  /// A pending timer is simply removed. An I/O operation may still be in the
  /// backend, using memory owned by its registration, so the registration
  /// stays until the operation's completion arrives; the backend is asked to
  /// finish it early, and the completion is discarded.
  pub(crate) fn cancel_stream(&self, id: u64) {
    {
      let mut inner = self.inner.borrow_mut();
      let inner = &mut *inner;
      let Some(reg) = inner.store.get_mut(id) else {
        return;
      };
      if inner.time.remove(id) {
        Self::retire(inner, id);
      } else {
        reg.cancel();
        inner.io.cancel(id);
        return;
      }
    }
    self.bury_and_wake();
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
      && let Some(detached) = inner.store.detach(id)
    {
      inner.dying.push(detached);
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
    // The backend has dropped its leases, so retire parks `other` in `dying`.
    // `lost` was detached and dropped with no slot release.
    backend.complete(lost, 0);
    backend.complete(other, 0);
    let mut inner = lio.inner.borrow_mut();
    drop(inner.store.detach(lost).unwrap().registration);
    Lio::retire(&mut inner, other);
  }

  #[test]
  #[should_panic(expected = "so a slot is never released")]
  fn releasing_a_slot_checks_every_retired_slot_is_tracked() {
    let (backend, lio) = lio(2);
    crate::api::nop().with_lio(&lio).when_done(|_| {});
    crate::api::nop().with_lio(&lio).when_done(|_| {});
    let (lost, other) = (backend.pushed()[0], backend.pushed()[1]);
    {
      let mut inner = lio.inner.borrow_mut();
      drop(inner.store.detach(lost).unwrap().registration);
      // Queued for release without the check `retire` makes.
      let detached = inner.store.detach(other).unwrap();
      inner.dying.push(detached);
    }
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

  #[test]
  fn a_waker_set_between_retiring_and_burying_is_woken_once_buried() {
    use std::sync::{Arc, mpsc};

    let (backend, lio) = lio(1);
    let (tx, rx) = mpsc::channel();
    lio
      .schedule_with(|arena| {
        crate::registration::Registration::new_waker_in(
          arena,
          Waker::noop().clone(),
          tx,
          crate::api::ops::Nop,
        )
      })
      .unwrap();
    let id = backend.pushed()[0];
    // The backend drops its lease on completion. Retire then parks the
    // registration in `dying` until bury.
    backend.complete(id, 0);
    Lio::retire(&mut lio.inner.borrow_mut(), id);

    // The channel is still open, so a stream polled now waits.
    let wakes = Arc::new(Wakes::default());
    lio.set_waker(id, Waker::from(wakes.clone()));
    assert_eq!(wakes.count(), 0, "woken before the channel closed");
    lio.bury_and_wake();
    assert_eq!(wakes.count(), 1);
    assert!(
      rx.try_recv().is_err_and(|e| e == mpsc::TryRecvError::Disconnected)
    );

    // With nothing left to drop, a waker for a gone operation is woken now.
    lio.set_waker(id, Waker::from(wakes.clone()));
    assert_eq!(wakes.count(), 2);
  }

  /// Reports a completion for every pushed op, twice.
  #[derive(Default)]
  struct Twice(Vec<u64>);

  impl IoBackend for Twice {
    fn init(&mut self, _: usize) -> io::Result<()> {
      Ok(())
    }
    fn push(
      &mut self,
      id: u64,
      _: crate::backend::Keep,
      _: &mut dyn crate::backend::loan::Loan,
      _: &mut Bump,
    ) {
      self.0.extend([id, id]);
    }
    fn flush(
      &mut self,
      _: &mut dyn crate::backend::loan::Loan,
    ) -> io::Result<()> {
      Ok(())
    }
    fn wait(
      &mut self,
      _: Option<Duration>,
      completed: &mut Vec<OpCompleted>,
      _: &mut dyn crate::backend::loan::Loan,
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

  /// Takes one more step, then finishes.
  struct TwoSteps(bool);

  impl crate::api::op::OpModel for TwoSteps {
    type Item = ();

    fn action(&mut self) -> Action<'_> {
      action_io!(crate::backend::op::Op::Nop)
    }

    fn complete(&mut self, _: Completion) -> crate::api::op::OpResult<()> {
      if std::mem::replace(&mut self.0, true) {
        crate::api::op::OpResult::Done(())
      } else {
        crate::api::op::OpResult::Again
      }
    }
  }

  impl crate::api::op::OneshotOpModel for TwoSteps {}

  /// Counts its wakes.
  #[derive(Default)]
  struct Wakes(std::sync::atomic::AtomicUsize);

  impl std::task::Wake for Wakes {
    fn wake(self: std::sync::Arc<Self>) {
      self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
  }

  impl Wakes {
    fn count(&self) -> usize {
      self.0.load(std::sync::atomic::Ordering::SeqCst)
    }
  }

  #[test]
  fn scheduling_on_a_full_driver_is_refused() {
    let (_, lio) = lio(1);
    crate::api::nop().with_lio(&lio).when_done(|_| {});
    let refused = lio.schedule_with(|arena| {
      Registration::new_callback_in(arena, |_| {}, crate::api::ops::Nop)
    });
    assert_eq!(refused.unwrap_err().kind(), io::ErrorKind::WouldBlock);
  }

  #[test]
  fn a_step_without_an_item_keeps_the_waker_for_the_last() {
    use std::future::{Future, IntoFuture};
    use std::task::Context;

    let (backend, lio) = lio(1);
    let wakes = std::sync::Arc::new(Wakes::default());
    let waker = Waker::from(wakes.clone());
    let mut future =
      crate::api::Io::from_op(TwoSteps(false)).with_lio(&lio).into_future();
    let mut cx = Context::from_waker(&waker);
    assert!(std::pin::Pin::new(&mut future).poll(&mut cx).is_pending());
    for _ in 0..2 {
      backend.complete_all(0);
      lio.try_run().unwrap();
    }
    assert_eq!(wakes.0.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(std::pin::Pin::new(&mut future).poll(&mut cx).is_ready());
  }

  /// Records where each step's first lowering allocation lands.
  #[derive(Clone, Default)]
  struct Lowering {
    inner: ScriptedBackend,
    first_allocs: std::rc::Rc<std::cell::RefCell<Vec<usize>>>,
  }

  impl IoBackend for Lowering {
    fn init(&mut self, cap: usize) -> io::Result<()> {
      self.inner.init(cap)
    }
    fn push(
      &mut self,
      id: u64,
      keep: crate::backend::Keep,
      loan: &mut dyn crate::backend::loan::Loan,
      bump: &mut Bump,
    ) {
      let at = bump.alloc(0u64) as *mut u64 as usize;
      self.first_allocs.borrow_mut().push(at);
      self.inner.push(id, keep, loan, bump);
    }
    fn flush(
      &mut self,
      loan: &mut dyn crate::backend::loan::Loan,
    ) -> io::Result<()> {
      self.inner.flush(loan)
    }
    fn drain(&mut self) -> bool {
      self.inner.drain()
    }
    fn wait(
      &mut self,
      timeout: Option<Duration>,
      completed: &mut Vec<OpCompleted>,
      loan: &mut dyn crate::backend::loan::Loan,
    ) -> io::Result<()> {
      self.inner.wait(timeout, completed, loan)
    }
  }

  #[test]
  fn every_step_lowers_into_its_slot_s_reset_arena() {
    let backend = Lowering::default();
    let lio = Lio::new_with_backend(backend.clone(), 1).unwrap();
    crate::api::Io::from_op(TwoSteps(false)).with_lio(&lio).when_done(|_| {});
    backend.inner.complete_all(0);
    lio.try_run().unwrap();
    let allocs = backend.first_allocs.borrow().clone();
    assert_eq!(allocs.len(), 2);
    assert_eq!(allocs[0], allocs[1], "the second step reused the slot's arena");
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
  }

  #[test]
  #[should_panic(expected = "queued a submission while a slot is free")]
  fn queueing_with_a_free_slot_panics() {
    let (_, lio) = lio(1);
    Lio::enqueue(&mut lio.inner.borrow_mut(), |_| unreachable!());
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

  impl IoBackend for Recording {
    fn init(&mut self, cap: usize) -> io::Result<()> {
      self.inner.init(cap)
    }
    fn resource_close(&self) -> crate::api::resource::ResourceClose {
      self.inner.resource_close()
    }
    fn push(
      &mut self,
      id: u64,
      keep: crate::backend::Keep,
      loan: &mut dyn crate::backend::loan::Loan,
      bump: &mut Bump,
    ) {
      self.inner.push(id, keep, loan, bump);
    }
    fn flush(
      &mut self,
      loan: &mut dyn crate::backend::loan::Loan,
    ) -> io::Result<()> {
      self.inner.flush(loan)
    }
    fn wait(
      &mut self,
      timeout: Option<Duration>,
      completed: &mut Vec<OpCompleted>,
      loan: &mut dyn crate::backend::loan::Loan,
    ) -> io::Result<()> {
      self.timeouts.borrow_mut().push(timeout);
      self.inner.wait(timeout, completed, loan)
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
  fn a_batch_of_256_stays_full_until_every_registration_is_dropped() {
    let full = full_while_burying(256);
    assert_eq!(full.len(), 256);
    assert!(
      full.iter().all(|full| *full),
      "releasing this batch waits until every registration is gone"
    );
  }

  #[test]
  fn a_batch_past_256_frees_a_chunk_while_later_registrations_drop() {
    let full = full_while_burying(257);
    assert_eq!(full.len(), 257);
    assert!(
      full[..64].iter().all(|full| *full),
      "the first chunk is still held while it is dropped"
    );
    assert!(
      !full[64],
      "the next registration is dropped after that chunk was released"
    );
  }

  /// Panics when the registration that captured it is dropped.
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
