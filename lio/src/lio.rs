use crate::{
  api::op::{Action, Completion},
  backend::{IoBackend, OpCompleted, store::OpStore},
  registration::Registration,
  time::TimeManager,
};

use std::{
  cell::RefCell,
  io,
  rc::Rc,
  task::Waker,
  time::{Duration, Instant},
};

use bumpalo::Bump;

/// Result code returned to userspace when a sleep timer fires.
/// Matches the expected error codes in TypedOp::extract_result for Sleep.
#[cfg(target_os = "linux")]
const SLEEP_RESULT: isize = -(libc::ETIME as isize);
#[cfg(any(
  target_os = "macos",
  target_os = "freebsd",
  target_os = "dragonfly"
))]
const SLEEP_RESULT: isize = -(libc::ETIMEDOUT as isize);
#[cfg(not(any(
  target_os = "linux",
  target_os = "macos",
  target_os = "freebsd",
  target_os = "dragonfly"
)))]
const SLEEP_RESULT: isize = 0;

thread_local! {
  static GLOBAL_LIO: RefCell<Option<Lio>> = const { RefCell::new(None) };
}

/// Installs a global Lio instance for the current thread.
///
/// After calling this, operations can be used without explicitly calling `.with_lio()`.
/// This is useful for thread-per-core designs where each thread has its own Lio instance.
///
/// # Panics
///
/// Panics if a global Lio is already installed on this thread.
pub fn install_global(lio: Lio) {
  GLOBAL_LIO.with(|global| {
    let mut global = global.borrow_mut();
    if global.is_some() {
      panic!("Global Lio already installed on this thread. Call uninstall_global() first.");
    }
    *global = Some(lio);
  });
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
  store: OpStore,
  io: Box<dyn IoBackend>,
  time: TimeManager,
  // Reused scratch vectors for `run_inner`.
  completed: Vec<OpCompleted>,
  expired_timers: Vec<u64>,
  to_dispatch: Vec<(u64, Action)>,
  to_remove: Vec<u64>,
  profile: Option<LioProfile>,
}

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
  /// * `cap` - The maximum number of concurrent operations
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
    #[cfg(any(
      target_os = "macos",
      target_os = "ios",
      target_os = "tvos",
      target_os = "watchos",
      target_os = "freebsd",
      target_os = "dragonfly",
      target_os = "openbsd",
      target_os = "netbsd"
    ))]
    {
      use crate::backend::impls::Poller;
      Self::new_with_backend(Poller::new(), cap)
    }
    #[cfg(target_os = "linux")]
    {
      use crate::backend::impls::IoUring;
      Self::new_with_backend(IoUring::new(), cap)
    }
  }

  /// Creates a new Lio driver with the specified backend and capacity.
  ///
  /// # Arguments
  ///
  /// * `backend` - The I/O backend implementation (e.g., io_uring, epoll/kqueue)
  /// * `cap` - The maximum number of concurrent operations
  pub fn new_with_backend<D>(mut backend: D, cap: usize) -> io::Result<Self>
  where
    D: IoBackend + 'static,
  {
    backend.init(cap)?;

    let inner = LioInner {
      io: Box::new(backend),
      store: OpStore::with_capacity(cap),
      time: TimeManager::with_capacity(cap),
      completed: Vec::with_capacity(cap),
      expired_timers: Vec::with_capacity(cap),
      to_dispatch: Vec::with_capacity(cap),
      to_remove: Vec::with_capacity(cap),
      profile: std::env::var_os("LIO_PROFILE").map(|_| LioProfile::default()),
    };
    Ok(Self { inner: Rc::new(RefCell::new(inner)) })
  }

  /// Schedule a registration built inside the store slot's persistent bump arena.
  pub(crate) fn schedule_with(
    &self,
    init: impl FnOnce(&mut Bump) -> Registration,
  ) -> io::Result<u64> {
    let mut inner = self.inner.borrow_mut();
    let id = inner.store.insert_with(init);
    let action =
      inner.store.get_mut(id).and_then(Registration::action).ok_or_else(
        || {
          io::Error::new(
            io::ErrorKind::InvalidInput,
            "OpModel returned no action on first call",
          )
        },
      )?;
    Self::dispatch_action(&mut inner, id, action);
    Ok(id)
  }

  /// Non-blocking poll for completed operations.
  ///
  /// Returns immediately, processing any completions that are ready.
  pub fn try_run(&self) -> io::Result<usize> {
    self.run_inner(Some(Duration::ZERO))
  }

  /// Block until at least one operation completes.
  pub fn run(&self) -> io::Result<usize> {
    self.run_inner(None)
  }

  /// Run the event loop with a timeout.
  ///
  /// Waits for at least one operation to complete or until the timeout expires,
  /// whichever comes first. Returns `Ok(true)` if completions were processed,
  /// `Ok(false)` if the timeout expired with no completions.
  pub fn run_timeout(&self, timeout: Duration) -> io::Result<usize> {
    self.run_inner(Some(timeout))
  }

  fn run_inner(&self, timeout: Option<Duration>) -> io::Result<usize> {
    let mut inner = self.inner.borrow_mut();
    let profiling_enabled = inner.profile.is_some();
    let mut flush_time = Duration::ZERO;
    let mut next_deadline_time = Duration::ZERO;
    let mut wait_time = Duration::ZERO;
    let mut completion_loop_time = Duration::ZERO;
    let mut completion_store_lookup_time = Duration::ZERO;
    let mut completion_on_completion_time = Duration::ZERO;
    let completion_is_finished_time = Duration::ZERO;
    let mut timer_poll_time = Duration::ZERO;
    let mut timer_loop_time = Duration::ZERO;
    let mut dispatch_time = Duration::ZERO;
    let mut remove_time = Duration::ZERO;
    let mut stale_completions = 0usize;
    let mut timer_expirations = 0usize;

    if profiling_enabled {
      let started = Instant::now();
      inner.io.flush()?;
      flush_time += started.elapsed();
    } else {
      inner.io.flush()?;
    }

    // Compute effective timeout: min of user timeout and next timer deadline
    let next_deadline = if profiling_enabled {
      let started = Instant::now();
      let deadline = inner.time.next_deadline();
      next_deadline_time += started.elapsed();
      deadline
    } else {
      inner.time.next_deadline()
    };
    let effective_timeout = match (timeout, next_deadline) {
      (Some(user), Some(timer)) => Some(user.min(timer)),
      (Some(user), None) => Some(user),
      (None, Some(timer)) => Some(timer),
      (None, None) => None,
    };

    let mut completed = std::mem::take(&mut inner.completed);
    let mut expired_timers = std::mem::take(&mut inner.expired_timers);
    let mut to_dispatch = std::mem::take(&mut inner.to_dispatch);
    let mut to_remove = std::mem::take(&mut inner.to_remove);

    completed.clear();
    expired_timers.clear();
    to_dispatch.clear();
    to_remove.clear();

    // Copy completion data to release borrow on inner.io
    // inner.completed.clear();
    if profiling_enabled {
      let started = Instant::now();
      inner.io.wait(effective_timeout, &mut completed)?;
      wait_time += started.elapsed();
    } else {
      inner.io.wait(effective_timeout, &mut completed)?;
    }

    let mut num_completed = 0;

    let completion_loop_started =
      if profiling_enabled { Some(Instant::now()) } else { None };
    for c in &completed {
      let id = c.registration_id();
      let result = c.result();

      let store_lookup_started =
        if profiling_enabled { Some(Instant::now()) } else { None };
      let Some(op) = inner.store.get_mut(id) else {
        if let Some(started) = store_lookup_started {
          completion_store_lookup_time += started.elapsed();
          stale_completions += 1;
        }
        continue;
      };
      if let Some(started) = store_lookup_started {
        completion_store_lookup_time += started.elapsed();
      }

      let on_completion_started =
        if profiling_enabled { Some(Instant::now()) } else { None };
      let completion_result = op.on_completion(Completion::new(result));
      if let Some(started) = on_completion_started {
        completion_on_completion_time += started.elapsed();
      }

      if let Some(next_action) = completion_result.next_action {
        to_dispatch.push((id, next_action));
      }
      if completion_result.done {
        to_remove.push(id);
      }

      num_completed += 1;
    }
    if let Some(started) = completion_loop_started {
      completion_loop_time += started.elapsed();
    }

    // Process expired timers
    if profiling_enabled {
      let started = Instant::now();
      expired_timers.extend(inner.time.poll_expired());
      timer_poll_time += started.elapsed();
      timer_expirations += expired_timers.len();
    } else {
      expired_timers.extend(inner.time.poll_expired());
    }
    let expired_timer_count = expired_timers.len();
    let timer_loop_started =
      if profiling_enabled { Some(Instant::now()) } else { None };
    for &timer_id in &expired_timers {
      let mut next_action = None;
      let mut finished = false;

      if let Some(reg) = inner.store.get_mut(timer_id) {
        let result = reg.on_completion(Completion::with_flags(
          SLEEP_RESULT,
          crate::api::op::CompletionFlags::TIMER,
        ));
        next_action = result.next_action;
        finished = result.done;
      }

      inner.time.remove(timer_id);

      if let Some(action) = next_action {
        to_dispatch.push((timer_id, action));
      }

      if finished {
        to_remove.push(timer_id);
      }
    }
    if let Some(started) = timer_loop_started {
      timer_loop_time += started.elapsed();
    }

    let dispatch_started =
      if profiling_enabled { Some(Instant::now()) } else { None };
    let dispatch_count = to_dispatch.len();
    for (id, action) in to_dispatch.drain(..) {
      Self::dispatch_action(&mut inner, id, action);
    }
    if let Some(started) = dispatch_started {
      dispatch_time += started.elapsed();
    }

    let remove_started =
      if profiling_enabled { Some(Instant::now()) } else { None };
    let remove_count = to_remove.len();
    for id in to_remove.drain(..) {
      inner.store.remove(id);
    }
    if let Some(started) = remove_started {
      remove_time += started.elapsed();
    }

    completed.clear();
    expired_timers.clear();
    to_dispatch.clear();
    to_remove.clear();
    inner.completed = completed;
    inner.expired_timers = expired_timers;
    inner.to_dispatch = to_dispatch;
    inner.to_remove = to_remove;

    if let Some(profile) = inner.profile.as_mut() {
      profile.run_inner_calls += 1;
      profile.completions_returned += num_completed;
      profile.stale_completions += stale_completions;
      profile.timer_expirations += timer_expirations;
      profile.dispatch_count += dispatch_count;
      profile.remove_count += remove_count;
      profile.flush_time += flush_time;
      profile.next_deadline_time += next_deadline_time;
      profile.wait_time += wait_time;
      profile.completion_loop_time += completion_loop_time;
      profile.completion_store_lookup_time += completion_store_lookup_time;
      profile.completion_on_completion_time += completion_on_completion_time;
      profile.completion_is_finished_time += completion_is_finished_time;
      profile.timer_poll_time += timer_poll_time;
      profile.timer_loop_time += timer_loop_time;
      profile.dispatch_time += dispatch_time;
      profile.remove_time += remove_time;
    }

    Ok(num_completed + expired_timer_count)
  }

  // NOTE: check_done and check_stream_done are no longer needed with the channel-based
  // architecture. Results are now sent through mpsc channels directly to futures/streams.

  pub(crate) fn set_waker(&self, id: u64, waker: Waker) {
    let mut inner = self.inner.borrow_mut();
    if let Some(entry) = inner.store.get_mut(id) {
      entry.set_waker(waker);
    }
  }

  /// Cancel an in-flight streaming operation.
  ///
  /// This is called when a stream is dropped to stop multishot operations.
  pub(crate) fn cancel_stream(&self, id: u64) {
    let mut inner = self.inner.borrow_mut();
    inner.time.remove(id);
    inner.store.remove(id);
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::{
    api::ops::Nop,
    backend::{IoBackend, OpCompleted, op::Op},
  };
  use std::sync::{Arc, Mutex};

  #[derive(Default)]
  struct TestBackend {
    queued: Vec<(u64, Op)>,
    completed: Vec<OpCompleted>,
  }

  impl IoBackend for TestBackend {
    fn init(&mut self, _cap: usize) -> io::Result<()> {
      Ok(())
    }

    fn push(&mut self, id: u64, op: Op, _step_bump: &mut Bump) {
      self.queued.push((id, op));
    }

    fn flush(&mut self) -> io::Result<()> {
      for (id, op) in self.queued.drain(..) {
        match op {
          Op::Nop => self.completed.push(OpCompleted::new(id, 0)),
          _ => unreachable!("test backend only supports Nop"),
        }
      }
      Ok(())
    }

    fn wait(
      &mut self,
      _timeout: Option<Duration>,
      completed: &mut Vec<OpCompleted>,
    ) -> io::Result<()> {
      completed.append(&mut self.completed);
      Ok(())
    }
  }

  #[test]
  fn schedule_and_run_completes_nop_callback() {
    let lio = Lio::new_with_backend(TestBackend::default(), 8).unwrap();
    let received = Arc::new(Mutex::new(Vec::new()));
    let out = Arc::clone(&received);
    let _id = lio
      .schedule_with(|arena| {
        Registration::new_callback_in(
          arena,
          move |item| out.lock().unwrap().push(item),
          Nop,
        )
      })
      .unwrap();

    let processed = lio.try_run().unwrap();
    assert_eq!(processed, 1);

    let items = received.lock().unwrap();
    assert_eq!(items.len(), 1);
    assert!(matches!(items[0], Ok(())));
  }
}
