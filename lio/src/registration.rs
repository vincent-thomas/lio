use std::collections::VecDeque;
use std::task::Waker;

use crate::api::op::{StreamOp, StreamResult, TypedOp};

/// Registration for tracking in-flight I/O operations.
///
/// Supports both single-shot operations (one completion) and streaming
/// operations (multiple completions from one submission).
// NOTE: Registration should **NEVER** impl Sync.
pub enum Registration {
  /// Single-shot operation that produces exactly one result.
  Single(SingleRegistration),
  /// Streaming operation that produces multiple results.
  Stream(StreamRegistration),
}

/// Registration for single-shot operations.
pub struct SingleRegistration {
  pub(crate) state: SingleState,
}

/// State machine for single-shot operations.
pub(crate) enum SingleState {
  /// Waiting for completion, will wake task.
  PendingWaker(Waker),
  /// Waiting for completion, will invoke callback.
  PendingCallback(SingleOpCallback),
  /// Completed, result waiting for check_done to consume.
  Completed(isize),
  /// Done: result delivered to user.
  Done,
}

/// Registration for streaming operations.
///
/// Supports two modes:
/// - Waker-based: results are queued and a waker is notified
/// - Callback-based: a callback is invoked for each completion
pub struct StreamRegistration {
  pub(crate) notifier: StreamNotifier,
  pub(crate) done: bool,
}

/// Notification mechanism for stream registrations.
pub(crate) enum StreamNotifier {
  /// Waker-based: queue results and wake the task.
  Waker { waker: Option<Waker>, results: VecDeque<isize> },
  /// Callback-based: invoke callback for each completion.
  Callback(StreamOpCallback),
}

impl StreamNotifier {
  /// Set waker for waker-based streams. No-op for callback-based.
  fn set_waker(&mut self, new_waker: Waker) {
    match self {
      Self::Waker { waker, results } => {
        if !results.is_empty() {
          // Has pending results, wake immediately
          new_waker.wake();
        } else {
          *waker = Some(new_waker);
        }
      }
      Self::Callback(_) => {
        // Callback streams don't use wakers
      }
    }
  }

  /// Notify about a completion.
  ///
  /// For waker-based: queues the result and wakes the task.
  /// For callback-based: invokes the callback with the extracted item.
  fn notify(&mut self, result: isize) {
    match self {
      Self::Waker { waker, results } => {
        results.push_back(result);
        if let Some(w) = waker.take() {
          w.wake();
        }
      }
      Self::Callback(cb) => {
        cb.call(result);
      }
    }
  }

  /// Pop a result from the queue (waker-based only).
  fn pop_result(&mut self) -> Option<isize> {
    match self {
      Self::Waker { results, .. } => results.pop_front(),
      Self::Callback(_) => {
        panic!("pop_result called on callback-based stream")
      }
    }
  }

  /// Check if results queue is empty.
  ///
  /// For callback-based streams, always returns true (results are
  /// consumed immediately).
  fn is_empty(&self) -> bool {
    match self {
      Self::Waker { results, .. } => results.is_empty(),
      Self::Callback(_) => true,
    }
  }
}

impl Registration {
  /// Create a waker-based registration for a single-shot operation.
  pub fn new_waker(waker: Waker) -> Self {
    Self::Single(SingleRegistration {
      state: SingleState::PendingWaker(waker),
    })
  }

  /// Create a waker-based registration for a streaming operation.
  pub fn new_stream_waker(waker: Waker) -> Self {
    Self::Stream(StreamRegistration {
      notifier: StreamNotifier::Waker {
        waker: Some(waker),
        results: VecDeque::new(),
      },
      done: false,
    })
  }

  /// Create a callback registration from an already-boxed TypedOp.
  ///
  /// TypedOp has already been boxed to establish a stable address before
  /// calling `into_op()`. This ensures pointers in the Op remain valid
  /// since the TypedOp won't move after boxing.
  ///
  /// Note: Callbacks are only supported for single-shot operations.
  pub fn new_callback<T, F>(callback: F, typed_op: Box<T>) -> Self
  where
    T: TypedOp,
    F: Fn(T::Result) + Send + 'static,
  {
    Self::Single(SingleRegistration {
      state: SingleState::PendingCallback(SingleOpCallback::new(callback, typed_op)),
    })
  }

  /// Create a callback registration for a streaming operation.
  ///
  /// The callback is called for each item yielded by the stream.
  pub fn new_stream_callback<T, F>(callback: F, stream_op: Box<T>) -> Self
  where
    T: StreamOp,
    F: Fn(T::Item) + Send + 'static,
  {
    Self::Stream(StreamRegistration {
      notifier: StreamNotifier::Callback(StreamOpCallback::new(callback, stream_op)),
      done: false,
    })
  }

  /// Sets the waker, replacing any existing waker.
  pub fn set_waker(&mut self, waker: Waker) {
    match self {
      Self::Single(inner) => {
        match &inner.state {
          SingleState::PendingWaker(_) => {
            inner.state = SingleState::PendingWaker(waker);
          }
          SingleState::Completed(_) | SingleState::Done => {
            // Already done, wake immediately
            waker.wake();
          }
          SingleState::PendingCallback(_) => {
            // Callback registrations don't use wakers
          }
        }
      }
      Self::Stream(inner) => {
        inner.notifier.set_waker(waker);
      }
    }
  }

  /// Called when an operation completes.
  ///
  /// For single-shot operations: stores the result and notifies.
  /// For streaming operations: queues the result and notifies (or calls callback).
  ///
  /// # Parameters
  ///
  /// - `result`: The operation result value
  /// - `more`: True if more completions are expected (for multishot ops)
  pub fn set_done(&mut self, result: isize, more: bool) {
    match self {
      Self::Single(inner) => {
        // Take ownership of current state to transition
        let state = std::mem::replace(&mut inner.state, SingleState::Done);
        inner.state = match state {
          SingleState::PendingWaker(waker) => {
            waker.wake();
            SingleState::Completed(result)
          }
          SingleState::PendingCallback(mut cb) => {
            cb.call(result);
            SingleState::Done
          }
          // Already completed/done - shouldn't happen but handle gracefully
          other => other,
        };
      }
      Self::Stream(inner) => {
        // Mark done if no more completions coming
        if !more {
          inner.done = true;
        }
        // Notify based on notifier type
        inner.notifier.notify(result);
      }
    }
  }

  /// Try to take a result from a single-shot operation.
  ///
  /// Returns `None` if the operation hasn't completed yet.
  /// Transitions Completed → Done.
  pub fn try_take_result(&mut self) -> Option<isize> {
    match self {
      Self::Single(inner) => {
        match &inner.state {
          SingleState::Completed(result) => {
            let result = *result;
            inner.state = SingleState::Done;
            Some(result)
          }
          _ => None,
        }
      }
      Self::Stream(_) => {
        panic!(
          "try_take_result called on stream registration - use try_take_stream_result instead"
        )
      }
    }
  }

  /// Try to take a result from a streaming operation.
  ///
  /// Returns `Some(result)` if there's a queued result,
  /// `None` if no results are available.
  ///
  /// # Panics
  ///
  /// Panics if called on a callback-based stream registration.
  pub fn try_take_stream_result(&mut self) -> Option<isize> {
    match self {
      Self::Single(_) => {
        panic!(
          "try_take_stream_result called on single registration - use try_take_result instead"
        )
      }
      Self::Stream(inner) => inner.notifier.pop_result(),
    }
  }

  /// Returns true if the stream is done (no more completions coming)
  /// and all results have been consumed.
  pub fn is_stream_done(&self) -> bool {
    match self {
      Self::Single(_) => false,
      Self::Stream(inner) => inner.done && inner.notifier.is_empty(),
    }
  }

  /// Returns true if this registration has completed and its result was consumed.
  ///
  /// For single-shot: true when state is Done.
  /// For streaming: true when done and all results consumed.
  #[allow(dead_code)]
  pub fn result_consumed(&self) -> bool {
    match self {
      Self::Single(inner) => matches!(inner.state, SingleState::Done),
      Self::Stream(inner) => inner.done && inner.notifier.is_empty(),
    }
  }

  /// Returns true when this registration can be removed from OpStore.
  ///
  /// For single-shot: state is Done.
  /// For streaming: done and all results consumed.
  pub fn is_finished(&self) -> bool {
    match self {
      Self::Single(inner) => matches!(inner.state, SingleState::Done),
      Self::Stream(inner) => inner.done && inner.notifier.is_empty(),
    }
  }

  /// Returns true if this is a stream registration.
  #[allow(dead_code)]
  pub fn is_stream(&self) -> bool {
    matches!(self, Self::Stream(_))
  }
}


/// Callback for single-shot operations (TypedOp).
///
/// Called exactly once when the operation completes. Consumes the op.
pub(crate) struct SingleOpCallback {
  callback: *const (),
  op: *mut (),
  call_fn: fn(*const (), *mut (), isize),
  drop_fn: fn(*const (), *mut ()),
}

impl Drop for SingleOpCallback {
  fn drop(&mut self) {
    if !self.op.is_null() {
      (self.drop_fn)(self.callback, self.op);
    }
  }
}

// SAFETY: SingleOpCallback is Send because:
// - The callback pointer points to a `F: Fn(T::Result) + Send` type
// - The op pointer points to a `T: TypedOp + Send` type
// - We maintain exclusive ownership
unsafe impl Send for SingleOpCallback {}

impl SingleOpCallback {
  pub(crate) fn new<T, F>(callback: F, typed_op: Box<T>) -> Self
  where
    T: TypedOp,
    F: Fn(T::Result) + Send + 'static,
  {
    SingleOpCallback {
      callback: Box::into_raw(Box::new(callback)) as *const (),
      op: Box::into_raw(typed_op) as *mut (),
      call_fn: Self::call_impl::<T, F>,
      drop_fn: Self::drop_impl::<T, F>,
    }
  }

  /// Call the callback with the operation result.
  ///
  /// Consumes the op (calls extract_result which takes self).
  /// Subsequent calls are no-ops.
  pub fn call(&mut self, res: isize) {
    if self.op.is_null() {
      return; // Already called
    }
    (self.call_fn)(self.callback, self.op, res);
    self.op = std::ptr::null_mut(); // Mark as consumed
  }

  fn call_impl<T, F>(callback_ptr: *const (), op_ptr: *mut (), res: isize)
  where
    T: TypedOp,
    F: Fn(T::Result),
  {
    // SAFETY: We consume the op here (extract_result takes self)
    let callback = unsafe { &*(callback_ptr as *const F) };
    let typed_op = unsafe { Box::from_raw(op_ptr as *mut T) };
    let result = typed_op.extract_result(res);
    callback(result);
  }

  fn drop_impl<T, F>(callback_ptr: *const (), op_ptr: *mut ())
  where
    T: TypedOp,
    F: Fn(T::Result),
  {
    unsafe {
      drop(Box::from_raw(callback_ptr as *mut F));
      drop(Box::from_raw(op_ptr as *mut T));
    }
  }
}

/// Callback for streaming operations (StreamOp).
///
/// Called multiple times, once for each completion.
pub(crate) struct StreamOpCallback {
  callback: *const (),
  op: *mut (),
  call_fn: fn(*const (), *mut (), isize),
  drop_fn: fn(*const (), *mut ()),
}

impl Drop for StreamOpCallback {
  fn drop(&mut self) {
    (self.drop_fn)(self.callback, self.op);
  }
}

// SAFETY: StreamOpCallback is Send because:
// - The callback pointer points to a `F: Fn(T::Item) + Send` type
// - The op pointer points to a `T: StreamOp + Send` type
// - We maintain exclusive ownership
unsafe impl Send for StreamOpCallback {}

impl StreamOpCallback {
  pub(crate) fn new<T, F>(callback: F, stream_op: Box<T>) -> Self
  where
    T: StreamOp,
    F: Fn(T::Item) + Send + 'static,
  {
    StreamOpCallback {
      callback: Box::into_raw(Box::new(callback)) as *const (),
      op: Box::into_raw(stream_op) as *mut (),
      call_fn: Self::call_impl::<T, F>,
      drop_fn: Self::drop_impl::<T, F>,
    }
  }

  /// Call the callback with the stream item result.
  ///
  /// Can be called multiple times (borrows the op via extract_item).
  pub fn call(&mut self, res: isize) {
    (self.call_fn)(self.callback, self.op, res);
  }

  fn call_impl<T, F>(callback_ptr: *const (), op_ptr: *mut (), res: isize)
  where
    T: StreamOp,
    F: Fn(T::Item),
  {
    let callback = unsafe { &*(callback_ptr as *const F) };
    let stream_op = unsafe { &mut *(op_ptr as *mut T) };
    if let StreamResult::Item(item) = stream_op.extract_item(res) {
      callback(item);
    }
  }

  fn drop_impl<T, F>(callback_ptr: *const (), op_ptr: *mut ())
  where
    T: StreamOp,
    F: Fn(T::Item),
  {
    unsafe {
      drop(Box::from_raw(callback_ptr as *mut F));
      drop(Box::from_raw(op_ptr as *mut T));
    }
  }
}

#[test]
fn test_op_reg_size() {
  // SingleState: largest variant is PendingCallback with 4 pointers (32 bytes) + discriminant
  assert_eq!(std::mem::size_of::<SingleState>(), 40);
}

#[test]
fn test_stream_registration() {
  use std::task::{RawWaker, RawWakerVTable};

  const VTABLE: RawWakerVTable =
    RawWakerVTable::new(|p| RawWaker::new(p, &VTABLE), |_| {}, |_| {}, |_| {});

  let waker =
    unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) };
  let mut reg = Registration::new_stream_waker(waker);

  // Initially no results
  assert!(!reg.is_stream_done());
  assert!(!reg.is_finished());

  // Add a result with more=true
  reg.set_done(42, true);
  assert!(!reg.is_stream_done());
  assert!(!reg.is_finished());

  // Take the result
  assert_eq!(reg.try_take_stream_result(), Some(42));
  assert!(!reg.is_stream_done()); // Still not done, more expected

  // Add final result with more=false
  reg.set_done(100, false);
  assert!(!reg.is_stream_done()); // Has pending result

  // Take the final result
  assert_eq!(reg.try_take_stream_result(), Some(100));
  assert!(reg.is_stream_done());
  assert!(reg.is_finished());
}
