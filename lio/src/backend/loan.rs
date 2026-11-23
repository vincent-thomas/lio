//! One-call view of an operation.
//!
//! This module is the implementor surface. Callers use [`crate::Lio`] and
//! [`crate::api`]. They do not build an [`OpView`] or a [`Loan`].
//!
//! [`IoBackend::push`](super::IoBackend::push) receives the [`OpView`] for
//! that call. [`flush`](super::IoBackend::flush) and
//! [`wait`](super::IoBackend::wait) receive a [`Loan`], because the backend
//! names an id and the driver still owns the model.
//!
//! [`OpView::op`] borrows the portable [`Op`](crate::backend::op::Op) for the
//! view. The backend copies a buffer address into its own kernel structure
//! before that borrow ends. This crate does not choose that format.

use super::op::Op;

/// The operation the driver shows for one call.
///
/// The borrow lasts for the view. [`op`](Self::op) is the portable operation.
/// A backend lowers it into its own kernel structure before the borrow ends.
pub struct OpView<'a> {
  op: &'a Op,
}

impl<'a> OpView<'a> {
  /// A view of `op`. The view ends when `op` does.
  pub fn new(op: &'a Op) -> Self {
    Self { op }
  }

  /// The portable operation for this call.
  ///
  /// Copy a buffer address into the backend's kernel structure before this
  /// borrow ends. [`drain`](super::IoBackend::drain) says when the kernel is
  /// finished with that address.
  pub fn op(&self) -> &Op {
    self.op
  }

  /// Byte `index` of a read, for this borrow only.
  ///
  /// # Safety
  ///
  /// The bytes are not borrowed anywhere else for this call. `OpView` is
  /// built from a shared `Op`, so two views do not keep each other out.
  pub unsafe fn read_buf(&mut self, index: usize) -> Option<&mut [u8]> {
    let op = self.op;
    let Op::Read { iovecs, iov_count, .. } = op else {
      return None;
    };
    if index >= *iov_count {
      return None;
    }
    // SAFETY: the descriptor array has `iov_count` live entries.
    let desc = unsafe { &*iovecs.as_ptr().add(index) };
    // SAFETY: that descriptor names bytes the model still owns.
    Some(unsafe { std::slice::from_raw_parts_mut(desc.as_ptr(), desc.len()) })
  }
}

/// Hands a backend an [`OpView`] for one callback.
///
/// The driver implements this. A backend calls [`lend`](Self::lend) when it
/// fills a submission or runs a syscall. It does not keep the view.
pub trait Loan {
  fn lend(&mut self, id: u64, f: &mut dyn for<'v> FnMut(&mut OpView<'v>));
}

impl<T: Loan + ?Sized> Loan for std::mem::ManuallyDrop<T> {
  fn lend(&mut self, id: u64, f: &mut dyn for<'v> FnMut(&mut OpView<'v>)) {
    (**self).lend(id, f)
  }
}

/// Test and contract harness. It holds the operations the test kept alive.
/// The backend never sees this map.
#[derive(Default)]
pub struct Attested {
  ops: std::collections::HashMap<u64, Op>,
}

impl Attested {
  pub fn new() -> Self {
    Self { ops: std::collections::HashMap::new() }
  }

  /// Remembers `op` under `id` until the test drops this value.
  ///
  /// # Safety
  ///
  /// `op`'s buffers stay allocated and unmoved until this value is dropped
  /// or [`take`](Self::take) removes `id`. Same rule as
  /// [`InFlight::attest`](super::InFlight::attest).
  pub unsafe fn hold(&mut self, id: u64, op: &Op) {
    self.ops.insert(id, op.clone());
  }

  pub fn take(&mut self, id: u64) -> Option<Op> {
    self.ops.remove(&id)
  }
}

thread_local! {
  static REMEMBERED: std::cell::RefCell<std::collections::HashMap<u64, Op>> =
    std::cell::RefCell::new(std::collections::HashMap::new());
}

impl Attested {
  /// Remembers `op` for later `flush` and `wait` in this thread.
  ///
  /// Contract tests lend again after `push` returns. The backend does not
  /// hold this map.
  ///
  /// # Safety
  ///
  /// Same as [`hold`](Self::hold): the buffers stay put until the test ends.
  pub unsafe fn remember(id: u64, op: &Op) {
    REMEMBERED.with(|map| {
      map.borrow_mut().insert(id, op.clone());
    });
  }
}

impl Loan for Attested {
  fn lend(&mut self, id: u64, f: &mut dyn for<'v> FnMut(&mut OpView<'v>)) {
    if let Some(op) = self.ops.get(&id) {
      let mut view = OpView::new(op);
      f(&mut view);
      return;
    }
    let op = REMEMBERED.with(|map| map.borrow().get(&id).cloned());
    let Some(op) = op else {
      panic!("no attested operation {id}");
    };
    let mut view = OpView::new(&op);
    f(&mut view);
  }
}
