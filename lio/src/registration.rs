use smallvec::SmallVec;
use std::task::Waker;

use crate::api::op::{StreamOp, StreamResult, TypedOp};

/// Inline storage for results - 1 slot covers single-shot; spills to heap for streams.
type ResultQueue = SmallVec<[isize; 2]>;

/// Registration for tracking in-flight I/O operations.
// NOTE: Registration should **NEVER** impl Sync.
pub struct Registration {
  state: State,
}

/// Unified state machine for I/O operations.
enum State {
  /// Waiting for completions, more may arrive.
  Waker { waker: Option<Waker>, results: ResultQueue },
  /// No more completions coming, draining queued results.
  Draining(ResultQueue),
  /// Callback-based operation.
  Callback(OpCallback),
  /// Finished.
  Done,
}

impl Registration {
  /// Create a waker-based registration.
  pub fn new_waker(waker: Waker) -> Self {
    Self {
      state: State::Waker { waker: Some(waker), results: SmallVec::new() },
    }
  }

  /// Create a callback registration for a single-shot operation.
  ///
  /// TypedOp has already been boxed to establish a stable address before
  /// calling `into_op()`. This ensures pointers in the Op remain valid
  /// since the TypedOp won't move after boxing.
  pub fn new_callback<T, F>(callback: F, typed_op: Box<T>) -> Self
  where
    T: TypedOp,
    F: Fn(T::Result) + Send + 'static,
  {
    Self { state: State::Callback(OpCallback::new_single(callback, typed_op)) }
  }

  /// Create a callback registration for a streaming operation.
  ///
  /// The callback is called for each item yielded by the stream.
  pub fn new_stream_callback<T, F>(callback: F, stream_op: Box<T>) -> Self
  where
    T: StreamOp,
    F: Fn(T::Item) + Send + 'static,
  {
    Self { state: State::Callback(OpCallback::new_stream(callback, stream_op)) }
  }

  /// Sets the waker, replacing any existing waker.
  pub fn set_waker(&mut self, waker: Waker) {
    match &mut self.state {
      State::Waker { waker: w, results } => {
        if results.is_empty() {
          *w = Some(waker);
        } else {
          waker.wake(); // Has pending results
        }
      }
      State::Draining(_) | State::Done => {
        waker.wake(); // Ready to consume or done
      }
      State::Callback(_) => {}
    }
  }

  /// Called when an operation completes.
  pub fn set_done(&mut self, result: isize, more: bool) {
    let state = std::mem::replace(&mut self.state, State::Done);
    self.state = match state {
      State::Waker { waker, mut results } => {
        results.push(result);
        if let Some(w) = waker {
          w.wake();
        }
        if more {
          State::Waker { waker: None, results }
        } else {
          State::Draining(results)
        }
      }
      State::Callback(mut cb) => {
        cb.call(result);
        if more { State::Callback(cb) } else { State::Done }
      }
      other => other,
    };
  }

  /// Try to take a result. Returns `None` if no results queued.
  pub fn try_take_result(&mut self) -> Option<isize> {
    match &mut self.state {
      State::Waker { results, .. } => {
        if results.is_empty() {
          None
        } else {
          Some(results.remove(0))
        }
      }
      State::Draining(results) => {
        if results.is_empty() {
          self.state = State::Done;
          None
        } else {
          let r = results.remove(0);
          if results.is_empty() {
            self.state = State::Done;
          }
          Some(r)
        }
      }
      State::Callback(_) => panic!("try_take_result on callback"),
      State::Done => None,
    }
  }

  /// Returns true if the stream is done (no more completions coming)
  /// and all results have been consumed.
  pub fn is_stream_done(&self) -> bool {
    matches!(self.state, State::Done)
  }

  /// Returns true when this registration can be removed from OpStore.
  ///
  /// For single-shot: state is Done.
  /// For streaming: done and all results consumed.
  pub fn is_finished(&self) -> bool {
    matches!(self.state, State::Done)
  }
}

/// Type-erased callback for I/O operations.
///
/// Works for both single-shot and streaming operations. The actual behavior
/// (result extraction) is determined by the call_fn provided during construction.
pub(crate) struct OpCallback {
  callback: *const (),
  op: *mut (),
  /// Called to invoke the callback. May set pointers to null if it consumes them.
  call_fn: fn(*mut *const (), *mut *mut (), isize),
  drop_fn: fn(*const (), *mut ()),
}

impl Drop for OpCallback {
  fn drop(&mut self) {
    // Only drop if not already consumed (single-shot sets to null after call)
    if !self.callback.is_null() {
      (self.drop_fn)(self.callback, self.op);
    }
  }
}

// SAFETY: OpCallback is Send because:
// - The callback pointer points to a `F: Fn(...) + Send` type
// - The op pointer points to a `T: TypedOp/StreamOp + Send` type
// - We maintain exclusive ownership
unsafe impl Send for OpCallback {}

impl OpCallback {
  /// Create a callback for a single-shot operation (TypedOp).
  pub(crate) fn new_single<T, F>(callback: F, typed_op: Box<T>) -> Self
  where
    T: TypedOp,
    F: Fn(T::Result) + Send + 'static,
  {
    OpCallback {
      callback: Box::into_raw(Box::new(callback)) as *const (),
      op: Box::into_raw(typed_op) as *mut (),
      call_fn: Self::call_single::<T, F>,
      drop_fn: Self::drop_impl::<T, F>,
    }
  }

  /// Create a callback for a streaming operation (StreamOp).
  pub(crate) fn new_stream<T, F>(callback: F, stream_op: Box<T>) -> Self
  where
    T: StreamOp,
    F: Fn(T::Item) + Send + 'static,
  {
    OpCallback {
      callback: Box::into_raw(Box::new(callback)) as *const (),
      op: Box::into_raw(stream_op) as *mut (),
      call_fn: Self::call_stream::<T, F>,
      drop_fn: Self::drop_stream::<T, F>,
    }
  }

  /// Call the callback with the operation result.
  ///
  /// For single-shot ops, this consumes the callback/op and sets pointers to null.
  /// For stream ops, this borrows the callback/op and leaves pointers unchanged.
  pub fn call(&mut self, res: isize) {
    (self.call_fn)(&mut self.callback, &mut self.op, res);
  }

  fn call_single<T, F>(
    callback_ptr: *mut *const (),
    op_ptr: *mut *mut (),
    res: isize,
  ) where
    T: TypedOp,
    F: Fn(T::Result),
  {
    // SAFETY: callback_ptr/op_ptr are valid pointers passed from call()
    let (cb, op) = unsafe { (*callback_ptr, *op_ptr) };
    // SAFETY: cb was created by Box::into_raw in new_single, taking ownership back
    let callback = unsafe { Box::from_raw(cb as *mut F) };
    // SAFETY: op was created by Box::into_raw in new_single, taking ownership back
    let typed_op = unsafe { Box::from_raw(op as *mut T) };
    // SAFETY: Mark as consumed before calling (prevents double-free if callback panics)
    unsafe {
      *callback_ptr = std::ptr::null();
      *op_ptr = std::ptr::null_mut();
    }
    let result = typed_op.extract_result(res);
    callback(result);
  }

  fn call_stream<T, F>(
    callback_ptr: *mut *const (),
    op_ptr: *mut *mut (),
    res: isize,
  ) where
    T: StreamOp,
    F: Fn(T::Item),
  {
    // SAFETY: callback_ptr/op_ptr are valid pointers passed from call()
    let (cb, op) = unsafe { (*callback_ptr, *op_ptr) };
    // SAFETY: cb was created by Box::into_raw in new_stream, borrowing not consuming
    let callback = unsafe { &*(cb as *const F) };
    // SAFETY: op was created by Box::into_raw in new_stream, borrowing mutably
    let stream_op = unsafe { &mut *(op as *mut T) };
    if let StreamResult::Item(item) = stream_op.extract_item(res) {
      callback(item);
    }
  }

  fn drop_impl<T, F>(callback_ptr: *const (), op_ptr: *mut ())
  where
    T: TypedOp,
    F: Fn(T::Result),
  {
    // SAFETY: Both pointers were created by Box::into_raw, we're dropping them
    unsafe {
      drop(Box::from_raw(callback_ptr as *mut F));
      drop(Box::from_raw(op_ptr as *mut T));
    }
  }

  fn drop_stream<T, F>(callback_ptr: *const (), op_ptr: *mut ())
  where
    T: StreamOp,
    F: Fn(T::Item),
  {
    // SAFETY: Both pointers were created by Box::into_raw, we're dropping them
    unsafe {
      drop(Box::from_raw(callback_ptr as *mut F));
      drop(Box::from_raw(op_ptr as *mut T));
    }
  }
}

#[test]
fn test_registration_size() {
  // State has 4 variants: Active, Draining, Callback, Done
  // Active is largest: Option<Waker>(16) + SmallVec<[isize;2]>(32) = 48 bytes
  assert_eq!(std::mem::size_of::<State>(), 48);
  assert_eq!(std::mem::size_of::<Registration>(), 48);
}

#[test]
fn test_single_registration() {
  use std::task::{RawWaker, RawWakerVTable};

  const VTABLE: RawWakerVTable =
    RawWakerVTable::new(|p| RawWaker::new(p, &VTABLE), |_| {}, |_| {}, |_| {});

  let waker =
    unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) };
  let mut reg = Registration::new_waker(waker);

  // Initially not finished
  assert!(!reg.is_finished());
  assert!(reg.try_take_result().is_none());

  // Complete the operation
  reg.set_done(42, false);
  assert!(!reg.is_finished()); // Not finished until result is taken

  // Take the result
  assert_eq!(reg.try_take_result(), Some(42));
  assert!(reg.is_finished());
}

#[test]
fn test_stream_registration() {
  use std::task::{RawWaker, RawWakerVTable};

  const VTABLE: RawWakerVTable =
    RawWakerVTable::new(|p| RawWaker::new(p, &VTABLE), |_| {}, |_| {}, |_| {});

  let waker =
    unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) };
  let mut reg = Registration::new_waker(waker);

  // Initially no results
  assert!(!reg.is_finished());

  // Add a result with more=true
  reg.set_done(42, true);
  assert!(!reg.is_finished());

  // Take the result
  assert_eq!(reg.try_take_result(), Some(42));
  assert!(!reg.is_finished()); // Still not done, more expected

  // Add final result with more=false
  reg.set_done(100, false);
  assert!(!reg.is_finished()); // Has pending result

  // Take the final result
  assert_eq!(reg.try_take_result(), Some(100));
  assert!(reg.is_finished());
}
