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

/// Where one operation's result goes: a callback placed in the operation's
/// arena, with its type erased down to the result it takes.
///
/// The operation's memory and its callback are written by different code:
/// the memory by the function that submits the operation, the callback by
/// whoever runs it. This is how the two meet without either naming the
/// other's type.
pub(crate) struct Callback<Out> {
  data: *mut (),
  call: fn(*mut (), Out),
  drop: fn(*mut ()),
}

impl<Out> Callback<Out> {
  #[inline]
  pub(crate) fn new_in<F>(arena: &mut Bump, f: F) -> Self
  where
    F: Fn(Out) + 'static,
  {
    Self {
      data: BumpBox::into_raw(BumpBox::new_in(f, arena)) as *mut (),
      call: |data, out| {
        // SAFETY: `data` was created from an `F` in `new_in` and is valid
        // until this `Callback` is dropped.
        let f = unsafe { &*(data as *const F) };
        f(out);
      },
      drop: |data| {
        // SAFETY: `data` was allocated by `BumpBox::into_raw` in `new_in`,
        // and `Callback::drop` runs this once.
        unsafe { drop(BumpBox::from_raw(data as *mut F)) };
      },
    }
  }

  #[inline]
  fn call(&self, out: Out) {
    (self.call)(self.data, out);
  }
}

impl<Out> Drop for Callback<Out> {
  fn drop(&mut self) {
    (self.drop)(self.data);
  }
}

/// Everything one operation needs, in its slot's arena: the memory the
/// backend uses, the two functions that speak for it, and where the result
/// goes.
struct Payload<S, Out> {
  state: S,
  action: ActionFn<S>,
  complete: CompleteFn<S, Out>,
  callback: Callback<Out>,
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
  /// `state` is the memory the operation uses. `action` builds what it
  /// submits from that memory, `complete` reads its result out of it, and
  /// `callback` receives that result.
  #[inline]
  pub(crate) fn new_in<S, Out>(
    arena: &mut Bump,
    state: S,
    action: ActionFn<S>,
    complete: CompleteFn<S, Out>,
    callback: Callback<Out>,
  ) -> Self
  where
    S: 'static,
    Out: 'static,
  {
    Self {
      payload: BumpBox::into_raw(BumpBox::new_in(
        Payload { state, action, complete, callback },
        arena,
      )) as *mut (),
      on_completion_fn: Self::on_completion_impl::<S, Out>,
      with_action_fn: Self::with_action_impl::<S, Out>,
      drop_fn: Self::drop_impl::<S, Out>,
      lease: crate::backend::Keep::new(),
    }
  }

  /// A nop whose result goes to `f`: the smallest registration there is.
  #[cfg(test)]
  pub(crate) fn nop_in<F>(arena: &mut Bump, f: F) -> Self
  where
    F: Fn(std::io::Result<()>) + 'static,
  {
    let callback = Callback::new_in(arena, f);
    Self::new_in(
      arena,
      (),
      |_| Action::nop(),
      |_, completion| completion.status(),
      callback,
    )
  }

  #[inline]
  fn with_action_impl<S, Out>(
    payload_ptr: *mut (),
    f: &mut dyn for<'a> FnMut(Action<'a>),
  ) {
    // SAFETY: `payload_ptr` was created from a `Payload<S, Out>` in `new_in`
    // and stays valid for the registration's lifetime.
    let payload = unsafe { &mut *(payload_ptr as *mut Payload<S, Out>) };
    f((payload.action)(&mut payload.state));
  }

  #[inline]
  fn on_completion_impl<S, Out>(payload_ptr: *mut (), completion: Completion) {
    // SAFETY: `payload_ptr` was created from a `Payload<S, Out>` in `new_in`
    // and stays valid while completions are dispatched.
    let payload = unsafe { &mut *(payload_ptr as *mut Payload<S, Out>) };
    let out = (payload.complete)(&mut payload.state, completion);
    payload.callback.call(out);
  }

  fn drop_impl<S, Out>(payload_ptr: *mut ()) {
    // SAFETY: `payload_ptr` was allocated by `BumpBox::into_raw` in `new_in`.
    unsafe {
      drop(BumpBox::from_raw(payload_ptr as *mut Payload<S, Out>));
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
  use std::cell::{Cell, RefCell};
  use std::rc::Rc;

  /// The memory of a nop that counts its drops.
  struct Counted {
    drops: Rc<Cell<usize>>,
  }

  impl Drop for Counted {
    fn drop(&mut self) {
      self.drops.set(self.drops.get() + 1);
    }
  }

  /// A registration over `Counted` that reports the raw result.
  fn counted(
    arena: &mut Bump,
    drops: &Rc<Cell<usize>>,
    f: impl Fn(isize) + 'static,
  ) -> Registration {
    let callback = Callback::new_in(arena, f);
    Registration::new_in(
      arena,
      Counted { drops: Rc::clone(drops) },
      |_| Action::nop(),
      |_, completion| completion.result(),
      callback,
    )
  }

  #[test]
  fn the_action_is_the_one_the_state_builds() {
    let mut arena = Bump::new();
    let mut reg = Registration::nop_in(&mut arena, |_| {});
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
    let mut reg =
      counted(&mut arena, &drops, move |result| sink.borrow_mut().push(result));

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
    let reg = counted(&mut arena, &drops, move |_| {
      let _held = &in_callback;
    });
    assert_eq!(Rc::strong_count(&held), 2);

    drop(reg);

    assert_eq!(drops.get(), 1);
    assert_eq!(Rc::strong_count(&held), 1);
  }
}
