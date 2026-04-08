use std::sync::mpsc;
use std::task::Waker;

use crate::api::op::{Action, Completion, OpModel, OpResult};

/// Result of processing a completion.
enum ProcessResult {
  /// The logical operation remains live and should perform another action.
  Continue(Action),
  /// The logical operation is complete.
  Done,
}

/// Waker-based result handler that sends typed results through a channel.
///
/// This handler manages an OpModel, processes completions, and sends
/// yielded items to a channel while waking the associated task.
pub(crate) struct WakerResultHandler {
  sender: *const (),
  op_model: *mut (),
  on_completion_fn: fn(*const (), *mut (), Completion) -> ProcessResult,
  action_fn: fn(*mut ()) -> Action,
  drop_fn: fn(*const (), *mut ()),
}

impl WakerResultHandler {
  fn new<T>(sender: mpsc::Sender<T::Item>, op_model: Box<T>) -> Self
  where
    T: OpModel,
  {
    Self {
      sender: Box::into_raw(Box::new(sender)) as *const (),
      op_model: Box::into_raw(op_model) as *mut (),
      on_completion_fn: Self::on_completion_impl::<T>,
      action_fn: Self::action_impl::<T>,
      drop_fn: Self::drop_impl::<T>,
    }
  }

  /// Get the next runtime action to perform.
  fn action(&mut self) -> Action {
    (self.action_fn)(self.op_model)
  }

  /// Process a completion result.
  ///
  /// Returns whether the operation is done and optionally an operation to resubmit.
  fn on_completion(&self, completion: Completion) -> ProcessResult {
    (self.on_completion_fn)(self.sender, self.op_model, completion)
  }

  fn action_impl<T: OpModel>(op_model_ptr: *mut ()) -> Action {
    let op_model = unsafe { &mut *(op_model_ptr as *mut T) };
    op_model.action()
  }

  fn on_completion_impl<T: OpModel>(
    sender_ptr: *const (),
    op_model_ptr: *mut (),
    completion: Completion,
  ) -> ProcessResult {
    let sender = unsafe { &*(sender_ptr as *const mpsc::Sender<T::Item>) };
    let op_model = unsafe { &mut *(op_model_ptr as *mut T) };

    match op_model.complete(completion) {
      OpResult::Again => {
        let next_action = op_model.action();
        ProcessResult::Continue(next_action)
      }
      OpResult::Yield(item) => {
        let _ = sender.send(item);
        let next_action = op_model.action();
        ProcessResult::Continue(next_action)
      }
      OpResult::Done(item) => {
        let _ = sender.send(item);
        ProcessResult::Done
      }
    }
  }

  fn drop_impl<T: OpModel>(sender_ptr: *const (), op_model_ptr: *mut ()) {
    unsafe {
      drop(Box::from_raw(sender_ptr as *mut mpsc::Sender<T::Item>));
      drop(Box::from_raw(op_model_ptr as *mut T));
    }
  }
}

impl Drop for WakerResultHandler {
  fn drop(&mut self) {
    (self.drop_fn)(self.sender, self.op_model);
  }
}

unsafe impl Send for WakerResultHandler {}

/// Type-erased callback that processes results and invokes user callback.
///
/// For callback-based OpModel operations, this:
/// 1. Calls op_model.complete() to interpret the completion and next step
/// 2. Invokes the user callback with yielded items
/// 3. Returns the next Op for resubmission if needed
pub(crate) struct OpCallback {
  callback: *const (),
  op_model: *mut (),
  on_completion_fn: fn(*const (), *mut (), Completion) -> ProcessResult,
  action_fn: fn(*mut ()) -> Action,
  drop_fn: fn(*const (), *mut ()),
}

impl OpCallback {
  fn new<T, F>(callback: F, op_model: Box<T>) -> Self
  where
    T: OpModel,
    F: Fn(T::Item) + Send + 'static,
  {
    Self {
      callback: Box::into_raw(Box::new(callback)) as *const (),
      op_model: Box::into_raw(op_model) as *mut (),
      on_completion_fn: Self::on_completion_impl::<T, F>,
      action_fn: Self::action_impl::<T>,
      drop_fn: Self::drop_impl::<T, F>,
    }
  }

  /// Get the next runtime action to perform.
  fn action(&mut self) -> Action {
    (self.action_fn)(self.op_model)
  }

  /// Process a completion: call complete() on OpModel and invoke callback with item.
  ///
  /// Returns whether the operation is done and optionally an operation to resubmit.
  fn on_completion(&self, completion: Completion) -> ProcessResult {
    (self.on_completion_fn)(self.callback, self.op_model, completion)
  }

  fn action_impl<T: OpModel>(op_model_ptr: *mut ()) -> Action {
    let op_model = unsafe { &mut *(op_model_ptr as *mut T) };
    op_model.action()
  }

  fn on_completion_impl<T, F>(
    callback_ptr: *const (),
    op_model_ptr: *mut (),
    completion: Completion,
  ) -> ProcessResult
  where
    T: OpModel,
    F: Fn(T::Item),
  {
    let callback = unsafe { &*(callback_ptr as *const F) };
    let op_model = unsafe { &mut *(op_model_ptr as *mut T) };

    match op_model.complete(completion) {
      OpResult::Again => {
        let next_action = op_model.action();
        ProcessResult::Continue(next_action)
      }
      OpResult::Yield(item) => {
        callback(item);
        let next_action = op_model.action();
        ProcessResult::Continue(next_action)
      }
      OpResult::Done(item) => {
        callback(item);
        ProcessResult::Done
      }
    }
  }

  fn drop_impl<T, F>(callback_ptr: *const (), op_model_ptr: *mut ())
  where
    T: OpModel,
    F: Fn(T::Item),
  {
    unsafe {
      drop(Box::from_raw(callback_ptr as *mut F));
      drop(Box::from_raw(op_model_ptr as *mut T));
    }
  }
}

impl Drop for OpCallback {
  fn drop(&mut self) {
    (self.drop_fn)(self.callback, self.op_model);
  }
}

// SAFETY: OpCallback is Send because both callback and OpModel are Send
unsafe impl Send for OpCallback {}

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
  /// Finished.
  Done,
}

impl Registration {
  /// Create a waker-based registration with OpModel and channel sender.
  ///
  /// The caller keeps the receiver to poll for typed results.
  /// Works for both single-shot and multi-shot operations.
  pub fn new_waker<T>(
    waker: Waker,
    sender: mpsc::Sender<T::Item>,
    op_model: Box<T>,
  ) -> Self
  where
    T: OpModel,
  {
    Self {
      state: State::Waker {
        waker: Some(waker),
        handler: WakerResultHandler::new(sender, op_model),
      },
    }
  }

  /// Create a callback registration for an operation.
  ///
  /// The callback is called for each item yielded by the operation.
  pub fn new_callback<T, F>(callback: F, op_model: Box<T>) -> Self
  where
    T: OpModel,
    F: Fn(T::Item) + Send + 'static,
  {
    Self { state: State::Callback(OpCallback::new(callback, op_model)) }
  }

  /// Get the initial action to perform.
  ///
  /// This should be called once when the registration is first created.
  pub fn action(&mut self) -> Option<Action> {
    match &mut self.state {
      State::Waker { handler, .. } => Some(handler.action()),
      State::Callback(cb) => Some(cb.action()),
      State::Done => None,
    }
  }

  /// Sets the waker, replacing any existing waker.
  pub fn set_waker(&mut self, waker: Waker) {
    match &mut self.state {
      State::Waker { waker: w, .. } => {
        *w = Some(waker);
      }
      State::Done => {
        waker.wake(); // Already done
      }
      State::Callback(_) => {}
    }
  }

  /// Called when an operation completes.
  ///
  /// Returns Some(action) if the model should continue, None otherwise.
  pub fn on_completion(&mut self, completion: Completion) -> Option<Action> {
    let state = std::mem::replace(&mut self.state, State::Done);

    match state {
      State::Waker { waker, handler } => {
        let result = handler.on_completion(completion);

        // Wake the waker
        if let Some(w) = waker {
          w.wake();
        }

        match result {
          ProcessResult::Continue(next_action) => {
            self.state = State::Waker { waker: None, handler };
            Some(next_action)
          }
          ProcessResult::Done => {
            self.state = State::Done;
            None
          }
        }
      }
      State::Callback(cb) => match cb.on_completion(completion) {
        ProcessResult::Continue(next_action) => {
          self.state = State::Callback(cb);
          Some(next_action)
        }
        ProcessResult::Done => {
          self.state = State::Done;
          None
        }
      },
      State::Done => {
        self.state = State::Done;
        None
      }
    }
  }

  /// Returns true when this registration can be removed from OpStore.
  ///
  /// For single-shot: state is Done.
  /// For streaming: done and all results consumed.
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
      let counter = unsafe { Arc::<AtomicUsize>::from_raw(data.cast()) };
      let cloned = Arc::clone(&counter);
      let _ = Arc::into_raw(counter);
      RawWaker::new(Arc::into_raw(cloned).cast(), &VTABLE)
    }

    unsafe fn wake(data: *const ()) {
      let counter = unsafe { Arc::<AtomicUsize>::from_raw(data.cast()) };
      counter.fetch_add(1, Ordering::SeqCst);
    }

    unsafe fn wake_by_ref(data: *const ()) {
      let counter = unsafe { Arc::<AtomicUsize>::from_raw(data.cast()) };
      counter.fetch_add(1, Ordering::SeqCst);
      let _ = Arc::into_raw(counter);
    }

    unsafe fn drop(data: *const ()) {
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
    let mut reg = Registration::new_callback(
      move |item| out.lock().unwrap().push(item),
      Box::new(Nop),
    );

    assert!(matches!(
      reg.action(),
      Some(Action::Io(crate::backend::op::Op::Nop))
    ));
    assert!(reg.on_completion(Completion::new(0)).is_none());
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
    let mut reg = Registration::new_waker(waker, tx, Box::new(Nop));

    assert!(matches!(
      reg.action(),
      Some(Action::Io(crate::backend::op::Op::Nop))
    ));
    assert!(reg.on_completion(Completion::new(0)).is_none());
    assert!(reg.is_finished());
    assert_eq!(wake_count.load(Ordering::SeqCst), 1);

    let item = rx.try_recv().expect("nop result should be sent");
    assert!(matches!(item, Ok(())));
  }

  #[test]
  fn callback_registration_again_then_done_stays_live_then_finishes() {
    let received = Arc::new(std::sync::Mutex::new(Vec::new()));
    let out = Arc::clone(&received);
    let mut reg = Registration::new_callback(
      move |item| out.lock().unwrap().push(item),
      Box::new(AgainThenDone { stage: 0 }),
    );

    assert!(matches!(
      reg.action(),
      Some(Action::Io(crate::backend::op::Op::Nop))
    ));
    assert!(matches!(
      reg.on_completion(Completion::new(0)),
      Some(Action::Io(crate::backend::op::Op::Nop))
    ));
    assert!(!reg.is_finished(), "Again must keep the registration live");

    assert!(reg.on_completion(Completion::new(1)).is_none());
    assert!(reg.is_finished(), "Done must finish the registration");

    let items = received.lock().unwrap();
    assert_eq!(items.as_slice(), &[7]);
  }

  #[test]
  fn callback_registration_yield_then_done_invokes_callback_each_time() {
    let received = Arc::new(std::sync::Mutex::new(Vec::new()));
    let out = Arc::clone(&received);
    let mut reg = Registration::new_callback(
      move |item| out.lock().unwrap().push(item),
      Box::new(YieldTwiceThenDone { stage: 0 }),
    );

    assert!(matches!(
      reg.action(),
      Some(Action::Io(crate::backend::op::Op::Nop))
    ));
    assert!(matches!(
      reg.on_completion(Completion::new(0)),
      Some(Action::Io(crate::backend::op::Op::Nop))
    ));
    assert!(!reg.is_finished(), "Yield must keep the registration live");

    assert!(matches!(
      reg.on_completion(Completion::new(1)),
      Some(Action::Io(crate::backend::op::Op::Nop))
    ));
    assert!(
      !reg.is_finished(),
      "subsequent Yield must keep the registration live"
    );

    assert!(reg.on_completion(Completion::new(2)).is_none());
    assert!(reg.is_finished());

    let items = received.lock().unwrap();
    assert_eq!(items.as_slice(), &[11, 13, 17]);
  }

  #[test]
  fn waker_registration_again_then_done_wakes_and_sends_on_terminal_only() {
    let wake_count = Arc::new(AtomicUsize::new(0));
    let waker = test_waker(Arc::clone(&wake_count));
    let (tx, rx) = mpsc::channel();
    let mut reg =
      Registration::new_waker(waker, tx, Box::new(AgainThenDone { stage: 0 }));

    assert!(matches!(
      reg.on_completion(Completion::new(0)),
      Some(Action::Io(crate::backend::op::Op::Nop))
    ));
    assert_eq!(wake_count.load(Ordering::SeqCst), 1);
    assert!(rx.try_recv().is_err(), "Again must not send a terminal item");
    assert!(!reg.is_finished());

    assert!(reg.on_completion(Completion::new(1)).is_none());
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
    let mut reg = Registration::new_waker(
      waker,
      tx,
      Box::new(YieldTwiceThenDone { stage: 0 }),
    );

    assert!(matches!(
      reg.on_completion(Completion::new(0)),
      Some(Action::Io(crate::backend::op::Op::Nop))
    ));
    assert_eq!(wake_count.load(Ordering::SeqCst), 1);
    assert_eq!(rx.try_recv().unwrap(), 11);
    assert!(!reg.is_finished());

    reg.set_waker(test_waker(Arc::clone(&wake_count)));
    assert!(matches!(
      reg.on_completion(Completion::new(1)),
      Some(Action::Io(crate::backend::op::Op::Nop))
    ));
    assert_eq!(wake_count.load(Ordering::SeqCst), 2);
    assert_eq!(rx.try_recv().unwrap(), 13);
    assert!(!reg.is_finished());

    reg.set_waker(test_waker(Arc::clone(&wake_count)));
    assert!(reg.on_completion(Completion::new(2)).is_none());
    assert_eq!(wake_count.load(Ordering::SeqCst), 3);
    assert_eq!(rx.try_recv().unwrap(), 17);
    assert!(reg.is_finished());
  }

  #[test]
  fn set_waker_on_done_wakes_immediately() {
    let wake_count = Arc::new(AtomicUsize::new(0));
    let waker = test_waker(Arc::clone(&wake_count));
    let (tx, _rx) = mpsc::channel();
    let mut reg = Registration::new_waker(waker, tx, Box::new(Nop));

    assert!(reg.on_completion(Completion::new(0)).is_none());
    assert!(reg.is_finished());

    reg.set_waker(test_waker(Arc::clone(&wake_count)));
    assert_eq!(wake_count.load(Ordering::SeqCst), 2);
  }

  #[test]
  fn done_registration_has_no_further_action_or_completion() {
    let mut reg =
      Registration::new_callback(|_: std::io::Result<()>| {}, Box::new(Nop));

    assert!(reg.on_completion(Completion::new(0)).is_none());
    assert!(reg.is_finished());
    assert!(reg.action().is_none());
    assert!(reg.on_completion(Completion::new(0)).is_none());
  }
}
