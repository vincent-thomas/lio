//! One-call view of an operation.
//!
//! A backend never receives an [`Op`](crate::backend::op::Op) it can store.
//! [`Loan::lend`] builds an [`OpView`] for the body of one callback. Safe
//! code only gets borrows that end with that callback. Any backend, in this
//! crate or outside it, gets the raw [`Op`] for that same call through
//! [`OpView::kernel`], so it can hand an address to the kernel or a syscall.
//! That address is not a value the view can keep.

use std::marker::PhantomData;

use super::op::Op;

/// The operation the driver is willing to show for one call.
///
/// The borrow lasts for the call. A safe caller cannot store it on the
/// backend: the only safe buffer access returns a slice tied to `&mut self`.
pub struct OpView<'a> {
  op: *const Op,
  _lt: PhantomData<&'a mut ()>,
}

impl<'a> OpView<'a> {
  pub(crate) fn new(op: &Op) -> Self {
    Self { op, _lt: PhantomData }
  }

  /// The operation, for this call only.
  ///
  /// # Safety
  ///
  /// Copy a buffer address into a kernel structure or pass it to a syscall
  /// before this call returns. Do not store it in the backend. The model
  /// behind the pointer stays pinned for the call, and the driver frees it
  /// only after [`drain`](super::IoBackend::drain) says the kernel is done.
  pub unsafe fn kernel(&self) -> &'a Op {
    // SAFETY: the driver built this view from a live model and lends it
    // only while that model is borrowed.
    unsafe { &*self.op }
  }

  /// Byte `index` of a read, for this borrow only.
  pub fn read_buf(&mut self, index: usize) -> Option<&mut [u8]> {
    // SAFETY: the model is borrowed for this view, and `&mut self` is the
    // only borrow of that buffer for this call.
    let op = unsafe { &*self.op };
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
  fn lend<'a>(&'a mut self, id: u64, f: &mut dyn FnMut(&mut OpView<'a>));
}

impl<T: Loan + ?Sized> Loan for std::mem::ManuallyDrop<T> {
  fn lend<'a>(&'a mut self, id: u64, f: &mut dyn FnMut(&mut OpView<'a>)) {
    (**self).lend(id, f)
  }
}

/// The one operation a `push` is submitting.
pub struct PushLoan<'b> {
  id: Option<u64>,
  op: &'b Op,
}

impl<'b> PushLoan<'b> {
  pub(crate) fn new(id: u64, op: &'b Op) -> Self {
    Self { id: Some(id), op }
  }

  /// One attested operation. The test chooses the id at `push`.
  pub(crate) fn any(op: &'b Op) -> Self {
    Self { id: None, op }
  }
}

impl Loan for PushLoan<'_> {
  fn lend<'a>(&'a mut self, id: u64, f: &mut dyn FnMut(&mut OpView<'a>)) {
    if let Some(expected) = self.id {
      assert_eq!(
        id, expected,
        "backend lent operation {id} while submitting {expected}"
      );
    }
    let mut view = OpView::new(self.op);
    f(&mut view);
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
  fn lend<'a>(&'a mut self, id: u64, f: &mut dyn FnMut(&mut OpView<'a>)) {
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
