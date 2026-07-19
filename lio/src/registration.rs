use std::sync::mpsc;
use std::task::Waker;

use bumpalo::Bump;
use bumpalo::boxed::Box as BumpBox;

use crate::api::op::{Action, Completion, OpModel, OpResult};

/// Result of processing a completion.
pub(crate) struct ProcessResult {
  pub(crate) next_action: Option<Action>,
}

impl ProcessResult {
  #[inline]
  fn continue_with(action: Action) -> Self {
    Self { next_action: Some(action) }
  }

  #[inline]
  fn done() -> Self {
    Self { next_action: None }
  }

  #[inline]
  pub(crate) fn is_done(&self) -> bool {
    self.next_action.is_none()
  }
}

/// Waker-based result handler that sends typed results through a channel.
///
/// This handler manages an OpModel, processes completions, and sends
/// yielded items to a channel while waking the associated task.
pub(crate) struct WakerResultHandler {
  payload: *mut (),
  on_completion_fn: fn(*mut (), Completion) -> ProcessResult,
  action_fn: fn(*mut ()) -> Action,
  drop_fn: fn(*mut ()),
}

struct WakerPayload<T: OpModel> {
  sender: mpsc::Sender<T::Item>,
  op_model: T,
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
        WakerPayload { sender, op_model },
        arena,
      )) as *mut (),
      on_completion_fn: Self::on_completion_impl::<T>,
      action_fn: Self::action_impl::<T>,
      drop_fn: Self::drop_bump_impl::<T>,
    }
  }

  /// Get the next runtime action to perform.
  #[inline]
  fn action(&mut self) -> Action {
    (self.action_fn)(self.payload)
  }

  /// Process a completion result.
  ///
  /// Returns whether the operation is done and optionally an operation to resubmit.
  #[inline]
  fn on_completion(&self, completion: Completion) -> ProcessResult {
    (self.on_completion_fn)(self.payload, completion)
  }

  #[inline]
  fn action_impl<T: OpModel>(payload_ptr: *mut ()) -> Action {
    // SAFETY: `payload_ptr` is created from `WakerPayload<T>` in `new`/`new_in`
    // and remains valid for the lifetime of this handler.
    let payload = unsafe { &mut *(payload_ptr as *mut WakerPayload<T>) };
    payload.op_model.action()
  }

  #[inline]
  fn on_completion_impl<T: OpModel>(
    payload_ptr: *mut (),
    completion: Completion,
  ) -> ProcessResult {
    // SAFETY: `payload_ptr` is created from `WakerPayload<T>` in `new`/`new_in`
    // and remains valid while completions are dispatched.
    let payload = unsafe { &mut *(payload_ptr as *mut WakerPayload<T>) };
    let sender = &payload.sender;
    let op_model = &mut payload.op_model;

    match op_model.complete(completion) {
      OpResult::Again => {
        let next_action = op_model.action();
        ProcessResult::continue_with(next_action)
      }
      OpResult::Yield(item) => {
        let _ = sender.send(item);
        let next_action = op_model.action();
        ProcessResult::continue_with(next_action)
      }
      OpResult::Done(item) => {
        let _ = sender.send(item);
        ProcessResult::done()
      }
    }
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
  action_fn: fn(*mut ()) -> Action,
  drop_fn: fn(*mut ()),
}

struct CallbackPayload<T: OpModel, F: Fn(T::Item)> {
  callback: F,
  op_model: T,
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
        CallbackPayload { callback, op_model },
        arena,
      )) as *mut (),
      on_completion_fn: Self::on_completion_impl::<T, F>,
      action_fn: Self::action_impl::<T, F>,
      drop_fn: Self::drop_bump_impl::<T, F>,
    }
  }

  /// Get the next runtime action to perform.
  #[inline]
  fn action(&mut self) -> Action {
    (self.action_fn)(self.payload)
  }

  /// Process a completion: call complete() on OpModel and invoke callback with item.
  ///
  /// Returns whether the operation is done and optionally an operation to resubmit.
  #[inline]
  fn on_completion(&self, completion: Completion) -> ProcessResult {
    (self.on_completion_fn)(self.payload, completion)
  }

  #[inline]
  fn action_impl<T: OpModel, F: Fn(T::Item)>(payload_ptr: *mut ()) -> Action {
    // SAFETY: `payload_ptr` is created from `CallbackPayload<T, F>` in
    // `new`/`new_in` and remains valid for the handler lifetime.
    let payload = unsafe { &mut *(payload_ptr as *mut CallbackPayload<T, F>) };
    payload.op_model.action()
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
    let callback = &payload.callback;
    let op_model = &mut payload.op_model;

    match op_model.complete(completion) {
      OpResult::Again => {
        let next_action = op_model.action();
        ProcessResult::continue_with(next_action)
      }
      OpResult::Yield(item) => {
        callback(item);
        let next_action = op_model.action();
        ProcessResult::continue_with(next_action)
      }
      OpResult::Done(item) => {
        callback(item);
        ProcessResult::done()
      }
    }
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

/// Registration for tracking in-flight I/O operations.
// NOTE: Registration should **NEVER** impl Sync.
pub struct Registration {
  state: State,
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
    }
  }

  /// Create a callback registration with payload storage in a bump arena.
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
    }
  }

  /// Get the initial action to perform.
  ///
  /// This should be called once when the registration is first created.
  #[inline]
  pub fn action(&mut self) -> Option<Action> {
    match &mut self.state {
      State::Waker { handler, .. } => Some(handler.action()),
      State::Callback(cb) => Some(cb.action()),
      #[cfg(test)]
      State::Done => None,
    }
  }

  /// Sets the waker, replacing any existing waker.
  pub fn set_waker(&mut self, waker: Waker) {
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

  /// Processes a driver-owned completion without materializing the terminal
  /// state, because the driver immediately removes terminal registrations.
  #[inline]
  pub(crate) fn on_driver_completion(
    &mut self,
    completion: Completion,
  ) -> ProcessResult {
    self.process_completion(completion)
  }

  #[inline]
  fn process_completion(&mut self, completion: Completion) -> ProcessResult {
    match &mut self.state {
      State::Waker { waker, handler } => {
        let result = handler.on_completion(completion);

        if let Some(w) = waker.take() {
          w.wake();
        }

        result
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

    fn action(&mut self) -> Action {
      Action::Io(crate::backend::op::Op::Nop)
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

    fn action(&mut self) -> Action {
      Action::Io(crate::backend::op::Op::Nop)
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
      Some(Action::Io(crate::backend::op::Op::Nop))
    ));
    assert!(reg.on_completion(Completion::new(0)).next_action.is_none());
    assert!(reg.is_finished());

    let items = received.lock().unwrap();
    assert_eq!(items.len(), 1);
    assert!(matches!(items[0], Ok(())));
  }

  #[test]
  fn waker_registration_completes_nop_and_wakes() {
    let wake_count = Arc::new(AtomicUsize::new(0));
    let waker = test_waker(Arc::clone(&wake_count));
    let (tx, rx) = mpsc::channel();
    let (_arena, mut reg) = new_waker_reg(waker, tx, Nop);

    assert!(matches!(
      reg.action(),
      Some(Action::Io(crate::backend::op::Op::Nop))
    ));
    assert!(reg.on_completion(Completion::new(0)).next_action.is_none());
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
      Some(Action::Io(crate::backend::op::Op::Nop))
    ));
    assert!(matches!(
      reg.on_completion(Completion::new(0)).next_action,
      Some(Action::Io(crate::backend::op::Op::Nop))
    ));
    assert!(!reg.is_finished(), "Again must keep the registration live");

    assert!(reg.on_completion(Completion::new(1)).next_action.is_none());
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
      Some(Action::Io(crate::backend::op::Op::Nop))
    ));
    assert!(matches!(
      reg.on_completion(Completion::new(0)).next_action,
      Some(Action::Io(crate::backend::op::Op::Nop))
    ));
    assert!(!reg.is_finished(), "Yield must keep the registration live");

    assert!(matches!(
      reg.on_completion(Completion::new(1)).next_action,
      Some(Action::Io(crate::backend::op::Op::Nop))
    ));
    assert!(
      !reg.is_finished(),
      "subsequent Yield must keep the registration live"
    );

    assert!(reg.on_completion(Completion::new(2)).next_action.is_none());
    assert!(reg.is_finished());

    let items = received.lock().unwrap();
    assert_eq!(items.as_slice(), &[11, 13, 17]);
  }

  #[test]
  fn waker_registration_again_then_done_wakes_and_sends_on_terminal_only() {
    let wake_count = Arc::new(AtomicUsize::new(0));
    let waker = test_waker(Arc::clone(&wake_count));
    let (tx, rx) = mpsc::channel();
    let (_arena, mut reg) =
      new_waker_reg(waker, tx, AgainThenDone { stage: 0 });

    assert!(matches!(
      reg.on_completion(Completion::new(0)).next_action,
      Some(Action::Io(crate::backend::op::Op::Nop))
    ));
    assert_eq!(wake_count.load(Ordering::SeqCst), 1);
    assert!(rx.try_recv().is_err(), "Again must not send a terminal item");
    assert!(!reg.is_finished());

    assert!(reg.on_completion(Completion::new(1)).next_action.is_none());
    assert_eq!(wake_count.load(Ordering::SeqCst), 1);
    let item = rx.try_recv().expect("Done result should be sent");
    assert_eq!(item, 7);
    assert!(reg.is_finished());
  }

  #[test]
  fn waker_registration_yield_then_done_sends_each_item_and_wakes_each_time() {
    let wake_count = Arc::new(AtomicUsize::new(0));
    let waker = test_waker(Arc::clone(&wake_count));
    let (tx, rx) = mpsc::channel();
    let (_arena, mut reg) =
      new_waker_reg(waker, tx, YieldTwiceThenDone { stage: 0 });

    assert!(matches!(
      reg.on_completion(Completion::new(0)).next_action,
      Some(Action::Io(crate::backend::op::Op::Nop))
    ));
    assert_eq!(wake_count.load(Ordering::SeqCst), 1);
    assert_eq!(rx.try_recv().unwrap(), 11);
    assert!(!reg.is_finished());

    reg.set_waker(test_waker(Arc::clone(&wake_count)));
    assert!(matches!(
      reg.on_completion(Completion::new(1)).next_action,
      Some(Action::Io(crate::backend::op::Op::Nop))
    ));
    assert_eq!(wake_count.load(Ordering::SeqCst), 2);
    assert_eq!(rx.try_recv().unwrap(), 13);
    assert!(!reg.is_finished());

    reg.set_waker(test_waker(Arc::clone(&wake_count)));
    assert!(reg.on_completion(Completion::new(2)).next_action.is_none());
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

    assert!(reg.on_completion(Completion::new(0)).next_action.is_none());
    assert!(reg.is_finished());

    reg.set_waker(test_waker(Arc::clone(&wake_count)));
    assert_eq!(wake_count.load(Ordering::SeqCst), 2);
  }

  #[test]
  fn done_registration_has_no_further_action_or_completion() {
    let (_arena, mut reg) = new_callback_reg(|_: std::io::Result<()>| {}, Nop);

    assert!(reg.on_completion(Completion::new(0)).next_action.is_none());
    assert!(reg.is_finished());
    assert!(reg.action().is_none());
    assert!(reg.on_completion(Completion::new(0)).next_action.is_none());
  }
}
