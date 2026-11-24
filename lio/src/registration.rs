use bumpalo::Bump;
use bumpalo::boxed::Box as BumpBox;

use crate::api::op::{Action, Completion};

/// Builds the action an operation submits from the memory it owns.
///
/// Pointers in the action borrow that memory. The driver keeps it in place,
/// untouched, until the backend has finished the submission.
pub(crate) type ActionFn<S> = for<'a> fn(&'a mut S) -> Action<'a>;

/// Reads an operation's result out of its memory, given what the backend
/// reported for the action [`ActionFn`] built from that same memory.
pub(crate) type CompleteFn<S, Out> = fn(&mut S, Completion) -> Out;

/// Calls the closure with the operation's action, for any lifetime of that
/// action.
type WithActionFn = fn(*mut (), &mut dyn for<'a> FnMut(Action<'a>));

/// Everything one operation needs, in its slot's arena: the memory the
/// backend uses, the two functions that speak for it, and where the result
/// goes.
struct Payload<S, Out, F> {
  state: S,
  action: ActionFn<S>,
  complete: CompleteFn<S, Out>,
  callback: F,
}

/// A copy of a registration's completion handler that can run without access
/// to the registration itself.
///
/// The handler's payload lives in the registration's arena, whose memory
/// never moves. The driver copies the handler out, releases its own borrow,
/// and only then runs it, because the handler runs user code (a callback, or
/// a model's `poll`) that may schedule, drop or drive operations on the same
/// `Lio`.
#[derive(Clone, Copy)]
pub(crate) struct RawHandler {
  payload: *mut (),
  on_completion_fn: fn(*mut (), Completion),
}

impl RawHandler {
  /// Reads the operation's result and delivers it.
  ///
  /// # Safety
  ///
  /// The registration this handler was copied from must be alive, must not
  /// be processed concurrently, and must not have been completed before.
  #[inline]
  pub(crate) unsafe fn complete(self, completion: Completion) {
    (self.on_completion_fn)(self.payload, completion);
  }
}

/// One operation in flight: submitted once, completed once.
// NOTE: Registration should **NEVER** impl Sync.
pub struct Registration {
  payload: *mut (),
  on_completion_fn: fn(*mut (), Completion),
  with_action_fn: WithActionFn,
  drop_fn: fn(*mut ()),
  /// The driver's share of the operation's lease. A backend holds the other
  /// shares.
  lease: crate::backend::Keep,
}

impl Registration {
  /// Creates a registration whose payload lives in a bump arena.
  ///
  /// `state` is the memory the operation uses. `action` and `complete` are
  /// that operation's two halves, and `callback` receives its result.
  #[inline]
  pub(crate) fn new_in<S, Out, F>(
    arena: &mut Bump,
    state: S,
    action: ActionFn<S>,
    complete: CompleteFn<S, Out>,
    callback: F,
  ) -> Self
  where
    S: 'static,
    Out: 'static,
    F: Fn(Out) + 'static,
  {
    Self {
      payload: BumpBox::into_raw(BumpBox::new_in(
        Payload { state, action, complete, callback },
        arena,
      )) as *mut (),
      on_completion_fn: Self::on_completion_impl::<S, Out, F>,
      with_action_fn: Self::with_action_impl::<S, Out, F>,
      drop_fn: Self::drop_impl::<S, Out, F>,
      lease: crate::backend::Keep::new(),
    }
  }

  #[inline]
  fn with_action_impl<S, Out, F>(
    payload_ptr: *mut (),
    f: &mut dyn for<'a> FnMut(Action<'a>),
  ) {
    // SAFETY: `payload_ptr` was created from a `Payload<S, Out, F>` in
    // `new_in` and stays valid for the registration's lifetime.
    let payload = unsafe { &mut *(payload_ptr as *mut Payload<S, Out, F>) };
    f((payload.action)(&mut payload.state));
  }

  #[inline]
  fn on_completion_impl<S, Out, F>(payload_ptr: *mut (), completion: Completion)
  where
    F: Fn(Out),
  {
    // SAFETY: `payload_ptr` was created from a `Payload<S, Out, F>` in
    // `new_in` and stays valid while completions are dispatched.
    let payload = unsafe { &mut *(payload_ptr as *mut Payload<S, Out, F>) };
    let out = (payload.complete)(&mut payload.state, completion);
    (payload.callback)(out);
  }

  fn drop_impl<S, Out, F>(payload_ptr: *mut ()) {
    // SAFETY: `payload_ptr` was allocated by `BumpBox::into_raw` in `new_in`.
    unsafe {
      drop(BumpBox::from_raw(payload_ptr as *mut Payload<S, Out, F>));
    }
  }

  /// A copy of the lease the driver holds for this operation.
  #[inline]
  pub(crate) fn lease(&self) -> crate::backend::Keep {
    self.lease.clone()
  }

  /// A backend still holds a copy of this operation's lease.
  #[inline]
  pub(crate) fn lease_held(&self) -> bool {
    self.lease.held_elsewhere()
  }

  /// Runs `f` on the operation's action while its memory is borrowed.
  ///
  /// The driver calls this once, when it submits the operation.
  #[inline]
  pub(crate) fn with_action(&mut self, f: &mut dyn for<'a> FnMut(Action<'a>)) {
    (self.with_action_fn)(self.payload, f);
  }

  /// The operation's action, with its borrow of the operation's memory
  /// erased, for a test to inspect.
  #[cfg(test)]
  pub(crate) fn action(&mut self) -> Option<Action<'static>> {
    let mut erased = None;
    self.with_action(&mut |action| {
      // SAFETY: tests inspect the action while this registration is alive
      // and never submit it.
      erased = Some(unsafe { action.erase_lifetime() });
    });
    erased
  }

  /// The handler to run, outside any driver borrow, for this operation's
  /// completion.
  #[inline]
  pub(crate) fn begin_completion(&mut self) -> RawHandler {
    RawHandler {
      payload: self.payload,
      on_completion_fn: self.on_completion_fn,
    }
  }
}

impl Drop for Registration {
  fn drop(&mut self) {
    (self.drop_fn)(self.payload);
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::api::ops::raw::Nop;
  use std::cell::{Cell, RefCell};
  use std::rc::Rc;

  /// A nop that counts its drops and reports the raw result.
  struct Counted {
    drops: Rc<Cell<usize>>,
  }

  impl Counted {
    fn action(&mut self) -> Action<'_> {
      Action::nop()
    }

    fn complete(&mut self, completion: Completion) -> isize {
      completion.result()
    }
  }

  impl Drop for Counted {
    fn drop(&mut self) {
      self.drops.set(self.drops.get() + 1);
    }
  }

  #[test]
  fn the_action_is_the_one_the_state_builds() {
    let mut arena = Bump::new();
    let mut reg =
      Registration::new_in(&mut arena, Nop, Nop::action, Nop::complete, |_| {});
    let mut seen = 0;
    reg.with_action(&mut |action| {
      assert!(action.is_nop());
      seen += 1;
    });
    assert_eq!(seen, 1);
  }

  #[test]
  fn a_completion_reaches_the_callback_as_the_operation_s_result() {
    let drops = Rc::new(Cell::new(0));
    let results = Rc::new(RefCell::new(Vec::new()));
    let sink = Rc::clone(&results);
    let mut arena = Bump::new();
    let mut reg = Registration::new_in(
      &mut arena,
      Counted { drops: Rc::clone(&drops) },
      Counted::action,
      Counted::complete,
      move |result| sink.borrow_mut().push(result),
    );

    let handler = reg.begin_completion();
    // SAFETY: `reg` is alive and this is its only completion.
    unsafe { handler.complete(Completion::new(7)) };

    assert_eq!(*results.borrow(), [7]);
    assert_eq!(drops.get(), 0, "the state lives until the registration drops");
    drop(reg);
    assert_eq!(drops.get(), 1);
  }

  #[test]
  fn dropping_an_uncompleted_registration_drops_state_and_callback() {
    let drops = Rc::new(Cell::new(0));
    let held = Rc::new(());
    let in_callback = Rc::clone(&held);
    let mut arena = Bump::new();
    let reg = Registration::new_in(
      &mut arena,
      Counted { drops: Rc::clone(&drops) },
      Counted::action,
      Counted::complete,
      move |_| {
        let _held = &in_callback;
      },
    );
    assert_eq!(Rc::strong_count(&held), 2);

    drop(reg);

    assert_eq!(drops.get(), 1);
    assert_eq!(Rc::strong_count(&held), 1);
  }
}
