use std::sync::mpsc;
use std::task::Waker;

use bumpalo::Bump;
use bumpalo::boxed::Box as BumpBox;

use crate::api::op::{Action, Completion, OpModel, OpResult};

/// Extends an action's borrow to `'static` while its model is still borrowed.
///
/// # Safety
///
/// The model that produced `action` must stay pinned, with its buffers
/// unmoved, until the backend finishes that submission.
#[cfg(test)]
unsafe fn erase_action(action: Action<'_>) -> Action<'static> {
  // SAFETY: the caller keeps the producing model pinned for the submission.
  unsafe { action.erase_lifetime() }
}

/// Result of processing a completion.
///
/// A continuation does not carry the next action. The driver asks the model
/// for that action while the model is still borrowed, and submits it in that
/// same borrow, so the operation's lifetime is the model's.
pub(crate) struct ProcessResult {
  continues: bool,
}

impl ProcessResult {
  #[inline]
  fn r#continue() -> Self {
    Self { continues: true }
  }

  #[inline]
  fn done() -> Self {
    Self { continues: false }
  }

  #[inline]
  pub(crate) fn is_done(&self) -> bool {
    !self.continues
  }
}

/// Calls the closure with the model's next action, for any lifetime of that action.
///
/// Returns whether the model still has an action to submit. A finished
/// model does not: asking it again would queue the same step forever.
type WithActionFn = fn(*mut (), &mut dyn for<'a> FnMut(Action<'a>)) -> bool;

/// Waker-based result handler that sends typed results through a channel.
///
/// This handler manages an OpModel, processes completions, and sends
/// yielded items to a channel while waking the associated task.
pub(crate) struct WakerResultHandler {
  payload: *mut (),
  on_completion_fn:
    fn(*mut (), Completion, &mut Option<Waker>) -> ProcessResult,
  #[cfg(test)]
  action_fn: fn(*mut ()) -> Action<'static>,
  with_action_fn: WithActionFn,
  drop_fn: fn(*mut ()),
}

struct WakerPayload<T: OpModel> {
  sender: mpsc::Sender<T::Item>,
  op_model: T,
  /// Set once `complete` returns [`OpResult::Done`]. The driver must not
  /// submit another action after that.
  finished: bool,
}

impl WakerResultHandler {
  #[inline]
  fn new_in<T>(
    arena: &mut Bump,
    sender: mpsc::Sender<T::Item>,
    op_model: T,
  ) -> Self
  where
    T: OpModel,
  {
    Self {
      payload: BumpBox::into_raw(BumpBox::new_in(
        WakerPayload { sender, op_model, finished: false },
        arena,
      )) as *mut (),
      on_completion_fn: Self::on_completion_impl::<T>,
      #[cfg(test)]
      action_fn: Self::action_impl::<T>,
      with_action_fn: Self::with_action_impl::<T>,
      drop_fn: Self::drop_bump_impl::<T>,
    }
  }

  /// Get the next runtime action to perform.
  #[cfg(test)]
  #[inline]
  fn action(&mut self) -> Action<'static> {
    (self.action_fn)(self.payload)
  }

  /// Runs `f` on the next action while the model that produced it is borrowed.
  #[inline]
  fn with_action(&mut self, f: &mut dyn for<'a> FnMut(Action<'a>)) -> bool {
    (self.with_action_fn)(self.payload, f)
  }

  /// Process a completion result.
  ///
  /// Returns whether the operation is done and optionally an operation to resubmit.
  #[cfg(test)]
  #[inline]
  fn on_completion(
    &self,
    completion: Completion,
    waker: &mut Option<Waker>,
  ) -> ProcessResult {
    (self.on_completion_fn)(self.payload, completion, waker)
  }

  #[cfg(test)]
  #[inline]
  fn action_impl<T: OpModel>(payload_ptr: *mut ()) -> Action<'static> {
    // SAFETY: `payload_ptr` is created from `WakerPayload<T>` in `new`/`new_in`
    // and remains valid for the lifetime of this handler.
    let payload = unsafe { &mut *(payload_ptr as *mut WakerPayload<T>) };
    // SAFETY: the model stays inside this handler, which the store keeps
    // pinned until the submitted action completes.
    unsafe { erase_action(payload.op_model.action()) }
  }

  #[inline]
  fn with_action_impl<T: OpModel>(
    payload_ptr: *mut (),
    f: &mut dyn for<'a> FnMut(Action<'a>),
  ) -> bool {
    // SAFETY: `payload_ptr` is created from `WakerPayload<T>` in `new`/`new_in`
    // and remains valid for the lifetime of this handler.
    let payload = unsafe { &mut *(payload_ptr as *mut WakerPayload<T>) };
    if payload.finished {
      panic!("finished model was submitted again");
    }
    f(payload.op_model.action());
    true
  }

  #[inline]
  fn on_completion_impl<T: OpModel>(
    payload_ptr: *mut (),
    completion: Completion,
    waker: &mut Option<Waker>,
  ) -> ProcessResult {
    // SAFETY: `payload_ptr` is created from `WakerPayload<T>` in `new`/`new_in`
    // and remains valid while completions are dispatched.
    let payload = unsafe { &mut *(payload_ptr as *mut WakerPayload<T>) };
    // `wake` is false for an internal step, and for a yield whose receiver
    // is already gone: there is nobody to tell.
    let (result, wake) = {
      let sender = &payload.sender;
      let op_model = &mut payload.op_model;
      match op_model.complete(completion) {
        OpResult::Again => {
          // The driver resubmits internal steps. Preserve the consumer's waker
          // until an item is available instead of making it poll an empty channel.
          // The next action is borrowed from the model when the driver submits.
          (ProcessResult::r#continue(), false)
        }
        OpResult::Yield(item) => {
          if sender.send(item).is_err() {
            // Every receiver is gone, so nothing can take another item: stop
            // instead of resubmitting forever.
            (ProcessResult::done(), false)
          } else {
            (ProcessResult::r#continue(), true)
          }
        }
        OpResult::Done(item) => {
          let _ = sender.send(item);
          (ProcessResult::done(), true)
        }
      }
    };
    if result.is_done() {
      payload.finished = true;
    }
    if wake && let Some(waker) = waker.take() {
      waker.wake();
    }
    result
  }

  fn drop_bump_impl<T: OpModel>(payload_ptr: *mut ()) {
    // SAFETY: `payload_ptr` was allocated by `BumpBox::into_raw` in `new_in`.
    unsafe {
      drop(BumpBox::from_raw(payload_ptr as *mut WakerPayload<T>));
    }
  }
}

impl Drop for WakerResultHandler {
  fn drop(&mut self) {
    (self.drop_fn)(self.payload);
  }
}

// SAFETY: the handler only stores an opaque payload pointer and function
// pointers; thread-safety requirements are carried by the concrete `OpModel`
// and channel sender captured in the payload constructors.
unsafe impl Send for WakerResultHandler {}

/// Type-erased callback that processes results and invokes user callback.
///
/// For callback-based OpModel operations, this:
/// 1. Calls op_model.complete() to interpret the completion and next step
/// 2. Invokes the user callback with yielded items
/// 3. Returns the next Op for resubmission if needed
pub(crate) struct OpCallback {
  payload: *mut (),
  on_completion_fn: fn(*mut (), Completion) -> ProcessResult,
  #[cfg(test)]
  action_fn: fn(*mut ()) -> Action<'static>,
  with_action_fn: WithActionFn,
  drop_fn: fn(*mut ()),
}

struct CallbackPayload<T: OpModel, F: Fn(T::Item)> {
  callback: F,
  op_model: T,
  /// Set once `complete` returns [`OpResult::Done`]. The driver must not
  /// submit another action after that.
  finished: bool,
}

impl OpCallback {
  #[inline]
  fn new_in<T, F>(arena: &mut Bump, callback: F, op_model: T) -> Self
  where
    T: OpModel,
    F: Fn(T::Item) + 'static,
  {
    Self {
      payload: BumpBox::into_raw(BumpBox::new_in(
        CallbackPayload { callback, op_model, finished: false },
        arena,
      )) as *mut (),
      on_completion_fn: Self::on_completion_impl::<T, F>,
      #[cfg(test)]
      action_fn: Self::action_impl::<T, F>,
      with_action_fn: Self::with_action_impl::<T, F>,
      drop_fn: Self::drop_bump_impl::<T, F>,
    }
  }

  /// Get the next runtime action to perform.
  #[cfg(test)]
  #[inline]
  fn action(&mut self) -> Action<'static> {
    (self.action_fn)(self.payload)
  }

  /// Runs `f` on the next action while the model that produced it is borrowed.
  #[inline]
  fn with_action(&mut self, f: &mut dyn for<'a> FnMut(Action<'a>)) -> bool {
    (self.with_action_fn)(self.payload, f)
  }

  /// Process a completion: call complete() on OpModel and invoke callback with item.
  ///
  /// Returns whether the operation is done and optionally an operation to resubmit.
  #[cfg(test)]
  #[inline]
  fn on_completion(&self, completion: Completion) -> ProcessResult {
    (self.on_completion_fn)(self.payload, completion)
  }

  #[cfg(test)]
  #[inline]
  fn action_impl<T: OpModel, F: Fn(T::Item)>(
    payload_ptr: *mut (),
  ) -> Action<'static> {
    // SAFETY: `payload_ptr` is created from `CallbackPayload<T, F>` in
    // `new`/`new_in` and remains valid for the handler lifetime.
    let payload = unsafe { &mut *(payload_ptr as *mut CallbackPayload<T, F>) };
    // SAFETY: the model stays inside this handler, which the store keeps
    // pinned until the submitted action completes.
    unsafe { erase_action(payload.op_model.action()) }
  }

  #[inline]
  fn with_action_impl<T, F>(
    payload_ptr: *mut (),
    f: &mut dyn for<'a> FnMut(Action<'a>),
  ) -> bool
  where
    T: OpModel,
    F: Fn(T::Item),
  {
    // SAFETY: `payload_ptr` is created from `CallbackPayload<T, F>` in
    // `new`/`new_in` and remains valid for the handler lifetime.
    let payload = unsafe { &mut *(payload_ptr as *mut CallbackPayload<T, F>) };
    if payload.finished {
      panic!("finished model was submitted again");
    }
    f(payload.op_model.action());
    true
  }

  #[inline]
  fn on_completion_impl<T, F>(
    payload_ptr: *mut (),
    completion: Completion,
  ) -> ProcessResult
  where
    T: OpModel,
    F: Fn(T::Item),
  {
    // SAFETY: `payload_ptr` is created from `CallbackPayload<T, F>` in
    // `new`/`new_in` and remains valid while completions are dispatched.
    let payload = unsafe { &mut *(payload_ptr as *mut CallbackPayload<T, F>) };
    let result = {
      let callback = &payload.callback;
      let op_model = &mut payload.op_model;
      match op_model.complete(completion) {
        OpResult::Again => ProcessResult::r#continue(),
        OpResult::Yield(item) => {
          callback(item);
          ProcessResult::r#continue()
        }
        OpResult::Done(item) => {
          callback(item);
          ProcessResult::done()
        }
      }
    };
    if result.is_done() {
      payload.finished = true;
    }
    result
  }

  fn drop_bump_impl<T, F>(payload_ptr: *mut ())
  where
    T: OpModel,
    F: Fn(T::Item),
  {
    // SAFETY: `payload_ptr` was allocated by `BumpBox::into_raw` in `new_in`.
    unsafe {
      drop(BumpBox::from_raw(payload_ptr as *mut CallbackPayload<T, F>));
    }
  }
}

impl Drop for OpCallback {
  fn drop(&mut self) {
    (self.drop_fn)(self.payload);
  }
}

/// A copy of a registration's completion handler that can run without access
/// to the registration itself.
///
/// The handler's payload (op model, channel or user callback) lives in the
/// registration's arena, whose memory never moves. The driver copies the
/// handler out, releases its own borrow, and only then runs it, because the
/// handler may run user code (a callback, or a waker that polls inline) that
/// schedules, drops or drives operations on the same `Lio`.
#[derive(Clone, Copy)]
pub(crate) enum RawHandler {
  Waker {
    payload: *mut (),
    on_completion_fn:
      fn(*mut (), Completion, &mut Option<Waker>) -> ProcessResult,
  },
  Callback {
    payload: *mut (),
    on_completion_fn: fn(*mut (), Completion) -> ProcessResult,
  },
}

impl RawHandler {
  /// Processes one completion: advances the op model and delivers any item
  /// (sending it and waking `waker`, or calling the user callback).
  ///
  /// `waker` is left in place when the operation continues without an item;
  /// the caller hands it back to the registration.
  ///
  /// # Safety
  ///
  /// The registration this handler was copied from must be alive, and must
  /// not be processed concurrently.
  #[inline]
  pub(crate) unsafe fn complete(
    self,
    completion: Completion,
    waker: &mut Option<Waker>,
  ) -> ProcessResult {
    match self {
      Self::Waker { payload, on_completion_fn } => {
        on_completion_fn(payload, completion, waker)
      }
      Self::Callback { payload, on_completion_fn } => {
        on_completion_fn(payload, completion)
      }
    }
  }
}

/// Registration for tracking in-flight I/O operations.
// NOTE: Registration should **NEVER** impl Sync.
pub struct Registration {
  state: State,
  /// Set when the operation's consumer is gone while the operation may still
  /// be in the backend. The registration (and the memory the backend uses)
  /// stays alive until the operation's completion arrives.
  cancelled: bool,
  /// The driver's share of the model lease. A backend holds the other shares.
  lease: crate::backend::Keep,
}

/// Unified state machine for I/O operations.
enum State {
  /// Waker-based operation (sends typed results through channel).
  Waker { waker: Option<Waker>, handler: WakerResultHandler },
  /// Callback-based operation (calls user callback with typed results).
  Callback(OpCallback),
  /// Finished state used by standalone registration tests. Driver-owned
  /// terminal registrations are removed immediately.
  #[cfg(test)]
  Done,
}

impl Registration {
  /// Create a waker-based registration with payload storage in a bump arena.
  ///
  /// Polled streams keep the waker in their own slot. Tests build this
  /// registration directly.
  #[cfg(test)]
  #[inline]
  pub fn new_waker_in<T>(
    arena: &mut Bump,
    waker: Waker,
    sender: mpsc::Sender<T::Item>,
    op_model: T,
  ) -> Self
  where
    T: OpModel,
  {
    Self {
      state: State::Waker {
        waker: Some(waker),
        handler: WakerResultHandler::new_in(arena, sender, op_model),
      },
      cancelled: false,
      lease: crate::backend::Keep::new(),
    }
  }

  /// Create a registration that only sends results through `sender`, with
  /// nobody to wake. It stops once every receiver is gone.
  #[inline]
  pub fn new_sender_in<T>(
    arena: &mut Bump,
    sender: mpsc::Sender<T::Item>,
    op_model: T,
  ) -> Self
  where
    T: OpModel,
  {
    Self {
      state: State::Waker {
        waker: None,
        handler: WakerResultHandler::new_in(arena, sender, op_model),
      },
      cancelled: false,
      lease: crate::backend::Keep::new(),
    }
  }

  /// Create a callback registration with payload storage in a bump arena.
  #[inline]
  pub fn new_callback_in<T, F>(
    arena: &mut Bump,
    callback: F,
    op_model: T,
  ) -> Self
  where
    T: OpModel,
    F: Fn(T::Item) + 'static,
  {
    Self {
      state: State::Callback(OpCallback::new_in(arena, callback, op_model)),
      cancelled: false,
      lease: crate::backend::Keep::new(),
    }
  }

  /// Get the initial action to perform.
  ///
  /// This should be called once when the registration is first created.
  /// Tests use it to inspect a step. The driver submits through
  /// [`with_action`](Self::with_action) so the action's lifetime stays tied
  /// to the model.
  #[cfg(test)]
  #[inline]
  pub fn action(&mut self) -> Option<Action<'static>> {
    match &mut self.state {
      State::Waker { handler, .. } => Some(handler.action()),
      State::Callback(cb) => Some(cb.action()),
      #[cfg(test)]
      State::Done => None,
    }
  }

  /// A copy of the lease the driver holds for this model.
  #[inline]
  pub(crate) fn lease(&self) -> crate::backend::Keep {
    self.lease.clone()
  }

  /// A backend still holds a copy of this model's lease.
  #[inline]
  pub(crate) fn lease_held(&self) -> bool {
    self.lease.held_elsewhere()
  }

  /// Runs `f` on the next action while the model that produced it is borrowed.
  ///
  /// Returns `false` when the registration is already finished.
  ///
  /// # Panics
  ///
  /// Panics if `complete` already finished the model. Submitting that model
  /// again would queue the same step forever.
  #[inline]
  pub(crate) fn with_action(
    &mut self,
    f: &mut dyn for<'a> FnMut(Action<'a>),
  ) -> bool {
    match &mut self.state {
      State::Waker { handler, .. } => handler.with_action(f),
      State::Callback(cb) => cb.with_action(f),
      #[cfg(test)]
      State::Done => false,
    }
  }

  /// Sets the waker, replacing any existing waker.
  #[cfg(test)]
  pub fn set_waker(&mut self, waker: Waker) {
    assert!(!self.cancelled, "registration: waker set after cancellation");
    match &mut self.state {
      State::Waker { waker: w, .. } => {
        *w = Some(waker);
      }
      #[cfg(test)]
      State::Done => {
        waker.wake(); // Already done
      }
      State::Callback(_) => {}
    }
  }

  /// Called when an operation completes.
  ///
  /// Returns Some(action) if the model should continue, None otherwise.
  #[cfg(test)]
  pub fn on_completion(&mut self, completion: Completion) -> ProcessResult {
    let result = self.process_completion(completion);
    if result.is_done() {
      self.state = State::Done;
    }
    result
  }

  /// Starts processing a completion: returns the handler to run (outside
  /// any driver borrow) and takes the task's waker, if any.
  #[inline]
  pub(crate) fn begin_completion(&mut self) -> (RawHandler, Option<Waker>) {
    match &mut self.state {
      State::Waker { waker, handler } => (
        RawHandler::Waker {
          payload: handler.payload,
          on_completion_fn: handler.on_completion_fn,
        },
        waker.take(),
      ),
      State::Callback(cb) => (
        RawHandler::Callback {
          payload: cb.payload,
          on_completion_fn: cb.on_completion_fn,
        },
        None,
      ),
      #[cfg(test)]
      State::Done => unreachable!("completed a finished registration"),
    }
  }

  /// Returns a waker that a completion did not consume, unless the task has
  /// registered a newer one meanwhile.
  /// Marks the operation as abandoned by its consumer, which will not be
  /// woken again. Its completion is still processed, so the op model can
  /// release what it produced, but leads to no further step.
  #[inline]
  pub(crate) fn cancel(&mut self) {
    self.cancelled = true;
    if let State::Waker { waker, .. } = &mut self.state {
      *waker = None;
    }
  }

  /// Takes the waker of a task still waiting on this operation.
  #[inline]
  pub(crate) fn take_waker(&mut self) -> Option<Waker> {
    match &mut self.state {
      State::Waker { waker, .. } => waker.take(),
      _ => None,
    }
  }

  /// Whether the operation was abandoned by its consumer.
  #[inline]
  pub(crate) fn is_cancelled(&self) -> bool {
    self.cancelled
  }

  #[inline]
  pub(crate) fn restore_waker(&mut self, waker: Waker) {
    assert!(!self.cancelled, "registration: waker restored after cancellation");
    if let State::Waker { waker: slot @ None, .. } = &mut self.state {
      *slot = Some(waker);
    }
  }

  #[cfg(test)]
  #[inline]
  fn process_completion(&mut self, completion: Completion) -> ProcessResult {
    match &mut self.state {
      State::Waker { waker, handler } => {
        handler.on_completion(completion, waker)
      }
      State::Callback(cb) => cb.on_completion(completion),
      #[cfg(test)]
      State::Done => ProcessResult::done(),
    }
  }

  #[cfg(test)]
  pub fn is_finished(&self) -> bool {
    matches!(self.state, State::Done)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::api::{
    op::{Completion, OpModel, OpResult},
    ops::Nop,
  };
  use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
  };
  use std::task::{RawWaker, RawWakerVTable};

  fn new_callback_reg<T, F>(callback: F, op_model: T) -> (Bump, Registration)
  where
    T: OpModel,
    F: Fn(T::Item) + 'static,
  {
    let mut arena = Bump::new();
    let reg = Registration::new_callback_in(&mut arena, callback, op_model);
    (arena, reg)
  }

  fn new_waker_reg<T>(
    waker: Waker,
    sender: mpsc::Sender<T::Item>,
    op_model: T,
  ) -> (Bump, Registration)
  where
    T: OpModel,
  {
    let mut arena = Bump::new();
    let reg = Registration::new_waker_in(&mut arena, waker, sender, op_model);
    (arena, reg)
  }

  struct AgainThenDone {
    stage: u8,
  }

  impl OpModel for AgainThenDone {
    type Item = i32;

    fn action(&mut self) -> Action<'_> {
      action_io!(crate::backend::op::Op::Nop)
    }

    fn complete(&mut self, completion: Completion) -> OpResult<Self::Item> {
      assert_eq!(completion.result, self.stage as isize);
      match self.stage {
        0 => {
          self.stage = 1;
          OpResult::Again
        }
        1 => {
          self.stage = 2;
          OpResult::Done(7)
        }
        _ => panic!("completed after terminal state"),
      }
    }
  }

  struct YieldTwiceThenDone {
    stage: u8,
  }

  impl OpModel for YieldTwiceThenDone {
    type Item = i32;

    fn action(&mut self) -> Action<'_> {
      action_io!(crate::backend::op::Op::Nop)
    }

    fn complete(&mut self, completion: Completion) -> OpResult<Self::Item> {
      assert_eq!(completion.result, self.stage as isize);
      match self.stage {
        0 => {
          self.stage = 1;
          OpResult::Yield(11)
        }
        1 => {
          self.stage = 2;
          OpResult::Yield(13)
        }
        2 => {
          self.stage = 3;
          OpResult::Done(17)
        }
        _ => panic!("completed after terminal state"),
      }
    }
  }

  fn test_waker(counter: Arc<AtomicUsize>) -> Waker {
    unsafe fn clone(data: *const ()) -> RawWaker {
      // SAFETY: `data` was produced by `Arc::into_raw` in `test_waker`.
      let counter = unsafe { Arc::<AtomicUsize>::from_raw(data.cast()) };
      let cloned = Arc::clone(&counter);
      let _ = Arc::into_raw(counter);
      RawWaker::new(Arc::into_raw(cloned).cast(), &VTABLE)
    }

    unsafe fn wake(data: *const ()) {
      // SAFETY: `data` was produced by `Arc::into_raw` in `test_waker`.
      let counter = unsafe { Arc::<AtomicUsize>::from_raw(data.cast()) };
      counter.fetch_add(1, Ordering::SeqCst);
    }

    unsafe fn wake_by_ref(data: *const ()) {
      // SAFETY: `data` was produced by `Arc::into_raw` in `test_waker`.
      let counter = unsafe { Arc::<AtomicUsize>::from_raw(data.cast()) };
      counter.fetch_add(1, Ordering::SeqCst);
      let _ = Arc::into_raw(counter);
    }

    unsafe fn drop(data: *const ()) {
      // SAFETY: `data` was produced by `Arc::into_raw` in `test_waker`.
      let _ = unsafe { Arc::<AtomicUsize>::from_raw(data.cast()) };
    }

    static VTABLE: RawWakerVTable =
      RawWakerVTable::new(clone, wake, wake_by_ref, drop);

    let raw = RawWaker::new(Arc::into_raw(counter).cast(), &VTABLE);
    // SAFETY: the vtable above maintains valid Arc ownership.
    unsafe { Waker::from_raw(raw) }
  }

  #[test]
  fn callback_registration_completes_nop() {
    let received = Arc::new(std::sync::Mutex::new(Vec::new()));
    let out = Arc::clone(&received);
    let (_arena, mut reg) =
      new_callback_reg(move |item| out.lock().unwrap().push(item), Nop);

    assert!(matches!(
      reg.action(),
      Some(action) if action.is_nop()
    ));
    assert!(reg.on_completion(Completion::new(0)).is_done());
    assert!(reg.is_finished());

    let items = received.lock().unwrap();
    assert_eq!(items.len(), 1);
    assert!(matches!(items[0], Ok(())));
  }

  #[test]
  fn a_finished_model_is_not_submitted_again() {
    let (_arena, mut reg) = new_callback_reg(|_| {}, Nop);
    assert!(reg.with_action(&mut |_| {}));
    // `process_completion` leaves the state as a live callback. Only the
    // finished flag can refuse the next action.
    assert!(reg.process_completion(Completion::new(0)).is_done());
    assert!(!reg.is_finished());
    let submitted =
      std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        reg.with_action(&mut |_| {})
      }));
    assert!(submitted.is_err(), "a finished callback model was submitted");

    let (tx, _rx) = mpsc::channel();
    let (_arena, mut reg) =
      new_waker_reg(test_waker(Arc::new(AtomicUsize::new(0))), tx, Nop);
    assert!(reg.process_completion(Completion::new(0)).is_done());
    let submitted =
      std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        reg.with_action(&mut |_| {})
      }));
    assert!(submitted.is_err(), "a finished waker model was submitted");
  }

  #[test]
  fn waker_registration_completes_nop_and_wakes() {
    let wake_count = Arc::new(AtomicUsize::new(0));
    let waker = test_waker(Arc::clone(&wake_count));
    let (tx, rx) = mpsc::channel();
    let (_arena, mut reg) = new_waker_reg(waker, tx, Nop);

    assert!(matches!(
      reg.action(),
      Some(action) if action.is_nop()
    ));
    assert!(reg.on_completion(Completion::new(0)).is_done());
    assert!(reg.is_finished());
    assert_eq!(wake_count.load(Ordering::SeqCst), 1);

    let item = rx.try_recv().expect("nop result should be sent");
    assert!(matches!(item, Ok(())));
  }

  #[test]
  fn callback_registration_again_then_done_stays_live_then_finishes() {
    let received = Arc::new(std::sync::Mutex::new(Vec::new()));
    let out = Arc::clone(&received);
    let (_arena, mut reg) = new_callback_reg(
      move |item| out.lock().unwrap().push(item),
      AgainThenDone { stage: 0 },
    );

    assert!(matches!(
      reg.action(),
      Some(action) if action.is_nop()
    ));
    assert!(!reg.on_completion(Completion::new(0)).is_done());
    assert!(matches!(reg.action(), Some(action) if action.is_nop()));
    assert!(!reg.is_finished(), "Again must keep the registration live");

    assert!(reg.on_completion(Completion::new(1)).is_done());
    assert!(reg.is_finished(), "Done must finish the registration");

    let items = received.lock().unwrap();
    assert_eq!(items.as_slice(), &[7]);
  }

  #[test]
  fn callback_registration_yield_then_done_invokes_callback_each_time() {
    let received = Arc::new(std::sync::Mutex::new(Vec::new()));
    let out = Arc::clone(&received);
    let (_arena, mut reg) = new_callback_reg(
      move |item| out.lock().unwrap().push(item),
      YieldTwiceThenDone { stage: 0 },
    );

    assert!(matches!(
      reg.action(),
      Some(action) if action.is_nop()
    ));
    assert!(!reg.on_completion(Completion::new(0)).is_done());
    assert!(matches!(reg.action(), Some(action) if action.is_nop()));
    assert!(!reg.is_finished(), "Yield must keep the registration live");

    assert!(!reg.on_completion(Completion::new(1)).is_done());
    assert!(matches!(reg.action(), Some(action) if action.is_nop()));
    assert!(
      !reg.is_finished(),
      "subsequent Yield must keep the registration live"
    );

    assert!(reg.on_completion(Completion::new(2)).is_done());
    assert!(reg.is_finished());

    let items = received.lock().unwrap();
    assert_eq!(items.as_slice(), &[11, 13, 17]);
  }

  #[test]
  fn waker_registration_again_preserves_waker_until_done() {
    let wake_count = Arc::new(AtomicUsize::new(0));
    let waker = test_waker(Arc::clone(&wake_count));
    let (tx, rx) = mpsc::channel();
    let (_arena, mut reg) =
      new_waker_reg(waker, tx, AgainThenDone { stage: 0 });

    assert!(!reg.on_completion(Completion::new(0)).is_done());
    assert!(matches!(reg.action(), Some(action) if action.is_nop()));
    assert_eq!(wake_count.load(Ordering::SeqCst), 0);
    assert!(rx.try_recv().is_err(), "Again must not send a terminal item");
    assert!(!reg.is_finished());

    assert!(reg.on_completion(Completion::new(1)).is_done());
    assert_eq!(wake_count.load(Ordering::SeqCst), 1);
    let item = rx.try_recv().expect("Done result should be sent");
    assert_eq!(item, 7);
    assert!(reg.is_finished());
  }

  #[test]
  fn waker_registration_repeated_again_preserves_waker_until_yield() {
    struct AgainThenYield(usize);

    impl OpModel for AgainThenYield {
      type Item = i32;

      fn action(&mut self) -> Action<'_> {
        action_io!(crate::backend::op::Op::Nop)
      }

      fn complete(&mut self, _: Completion) -> OpResult<i32> {
        self.0 += 1;
        match self.0 {
          1..=8 => OpResult::Again,
          9 => OpResult::Yield(11),
          _ => OpResult::Done(17),
        }
      }
    }

    let wake_count = Arc::new(AtomicUsize::new(0));
    let (tx, rx) = mpsc::channel();
    let (_arena, mut reg) =
      new_waker_reg(test_waker(Arc::clone(&wake_count)), tx, AgainThenYield(0));
    for _ in 0..8 {
      assert!(!reg.on_completion(Completion::new(0)).is_done());
      assert_eq!(wake_count.load(Ordering::SeqCst), 0);
      assert!(matches!(rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
    }
    assert!(!reg.on_completion(Completion::new(0)).is_done());
    assert_eq!(wake_count.load(Ordering::SeqCst), 1);
    assert_eq!(rx.try_recv().unwrap(), 11);

    reg.set_waker(test_waker(Arc::clone(&wake_count)));
    assert!(reg.on_completion(Completion::new(0)).is_done());
    assert_eq!(wake_count.load(Ordering::SeqCst), 2);
    assert_eq!(rx.try_recv().unwrap(), 17);
  }

  #[test]
  fn waker_registration_yield_then_done_sends_each_item_and_wakes_each_time() {
    let wake_count = Arc::new(AtomicUsize::new(0));
    let waker = test_waker(Arc::clone(&wake_count));
    let (tx, rx) = mpsc::channel();
    let (_arena, mut reg) =
      new_waker_reg(waker, tx, YieldTwiceThenDone { stage: 0 });

    assert!(!reg.on_completion(Completion::new(0)).is_done());
    assert!(matches!(reg.action(), Some(action) if action.is_nop()));
    assert_eq!(wake_count.load(Ordering::SeqCst), 1);
    assert_eq!(rx.try_recv().unwrap(), 11);
    assert!(!reg.is_finished());

    reg.set_waker(test_waker(Arc::clone(&wake_count)));
    assert!(!reg.on_completion(Completion::new(1)).is_done());
    assert!(matches!(reg.action(), Some(action) if action.is_nop()));
    assert_eq!(wake_count.load(Ordering::SeqCst), 2);
    assert_eq!(rx.try_recv().unwrap(), 13);
    assert!(!reg.is_finished());

    reg.set_waker(test_waker(Arc::clone(&wake_count)));
    assert!(reg.on_completion(Completion::new(2)).is_done());
    assert_eq!(wake_count.load(Ordering::SeqCst), 3);
    assert_eq!(rx.try_recv().unwrap(), 17);
    assert!(reg.is_finished());
  }

  #[test]
  fn set_waker_on_done_wakes_immediately() {
    let wake_count = Arc::new(AtomicUsize::new(0));
    let waker = test_waker(Arc::clone(&wake_count));
    let (tx, _rx) = mpsc::channel();
    let (_arena, mut reg) = new_waker_reg(waker, tx, Nop);

    assert!(reg.on_completion(Completion::new(0)).is_done());
    assert!(reg.is_finished());

    reg.set_waker(test_waker(Arc::clone(&wake_count)));
    assert_eq!(wake_count.load(Ordering::SeqCst), 2);
  }

  #[test]
  fn done_registration_has_no_further_action_or_completion() {
    let (_arena, mut reg) = new_callback_reg(|_: std::io::Result<()>| {}, Nop);

    assert!(reg.on_completion(Completion::new(0)).is_done());
    assert!(reg.is_finished());
    assert!(reg.action().is_none());
    assert!(reg.on_completion(Completion::new(0)).is_done());
  }

  #[test]
  fn dropping_a_registration_drops_its_payload() {
    let (tx, rx) = mpsc::channel::<std::io::Result<()>>();
    let (_arena, reg) = new_waker_reg(Waker::noop().clone(), tx, Nop);
    drop(reg);
    assert!(matches!(rx.try_recv(), Err(mpsc::TryRecvError::Disconnected)));

    let (tx, rx) = mpsc::channel::<()>();
    let (_arena, reg) = new_callback_reg(
      move |_| {
        let _ = &tx;
      },
      Nop,
    );
    drop(reg);
    assert!(matches!(rx.try_recv(), Err(mpsc::TryRecvError::Disconnected)));
  }

  #[test]
  #[should_panic(expected = "waker set after cancellation")]
  fn setting_a_waker_after_cancellation_panics() {
    let (_arena, mut reg) = new_callback_reg(|_| {}, Nop);
    reg.cancel();
    reg.set_waker(Waker::noop().clone());
  }
}
