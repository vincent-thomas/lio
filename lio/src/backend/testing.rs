//! Backends for testing lio's bookkeeping without doing any I/O.
//!
//! - [`ScriptedBackend`] performs nothing on its own: the test decides which
//!   operations complete, in which order and with which result. By default it
//!   also touches the memory of every operation when it completes or is
//!   drained, the way the kernel would, so a buffer freed or reused too early
//!   shows up under Miri or a sanitizer.
//! - [`Checked`] wraps any backend and panics as soon as either side breaks
//!   the [`IoBackend`] contract.
//!
//! ```
//! use lio::Lio;
//! use lio::backend::testing::{Checked, ScriptedBackend};
//!
//! let backend = ScriptedBackend::new();
//! let lio = Lio::new_with_backend(Checked::new(backend.clone()), 4).unwrap();
//!
//! lio::api::nop().with_lio(&lio).when_done(|res| assert!(res.is_ok()));
//! lio.try_run().unwrap();
//!
//! let id = backend.in_flight()[0];
//! backend.complete(id, 0);
//! lio.try_run().unwrap();
//! assert!(backend.in_flight().is_empty());
//! ```

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::hint::black_box;
use std::io;
use std::ptr;
use std::rc::Rc;
use std::time::Duration;

use bumpalo::Bump;

use crate::api::resource::ResourceClose;
use crate::backend::op::{MsgBuf, MsgBufMut, Op};
use crate::backend::{IoBackend, OpCompleted};

/// The byte [`ScriptedBackend`] writes into the buffers it fills.
pub const POISON: u8 = 0xA5;

#[derive(Default)]
struct Script {
  init: Option<usize>,
  /// Every id ever pushed, in push order.
  pushed: Vec<u64>,
  /// Pushed and not yet completed, in push order.
  in_flight: Vec<(u64, Op)>,
  /// The tickets of `in_flight`, given up with those operations.
  tickets: HashMap<u64, crate::backend::Ticket>,
  cancelled: Vec<u64>,
  /// Completed, waiting for the next `wait()`.
  ready: VecDeque<OpCompleted>,
  auto: Option<isize>,
  stuck: bool,
  no_poison: bool,
  /// `drain` claims success without forgetting stored operations.
  drain_while_holding: bool,
}

impl Drop for Script {
  fn drop(&mut self) {
    for (_, ticket) in self.tickets.drain() {
      // SAFETY: no kernel or thread ever sees the addresses, and this
      // backend is gone: nothing uses them.
      unsafe { ticket.release() };
    }
  }
}

/// A backend that only does what the test tells it to.
///
/// Clones share their state: hand one clone to [`Lio`](crate::Lio) and keep
/// another to drive it. Pushed operations stay in flight until
/// [`complete`](Self::complete) is called for them; the completion is then
/// reported by the next `wait()`. `wait()` never blocks.
///
/// Operations do not refer to real descriptors, so the resources they carry
/// are never closed.
#[derive(Clone, Default)]
pub struct ScriptedBackend(Rc<RefCell<Script>>);

impl ScriptedBackend {
  /// A backend that holds every operation until the test completes it.
  pub fn new() -> Self {
    Self::default()
  }

  /// A backend that completes every operation with `result` when it is
  /// flushed, in push order.
  pub fn auto(result: isize) -> Self {
    let backend = Self::new();
    backend.0.borrow_mut().auto = Some(result);
    backend
  }

  /// Stops touching operation memory on completion and drain.
  ///
  /// Only for tests that check buffer contents the scripted results do not
  /// fill in.
  ///
  /// # Safety
  ///
  /// Without the poison, nothing writes the memory a scripted result claims
  /// was written. Every operation completed with a non-negative result must
  /// point at memory that is already initialized for at least that many
  /// bytes.
  pub unsafe fn without_poison(self) -> Self {
    self.0.borrow_mut().no_poison = true;
    self
  }

  /// Makes [`drain`](IoBackend::drain) report operations still running, so
  /// the driver has to leak their memory.
  pub fn set_stuck(&self, stuck: bool) {
    self.0.borrow_mut().stuck = stuck;
  }

  /// Makes [`drain`](IoBackend::drain) report success while it still stores
  /// every in-flight operation, caller buffer addresses included.
  ///
  /// That report is rejected. `drain` returning true is a checked promise,
  /// not a proof the type system can make.
  pub fn drain_while_holding(&self) {
    self.0.borrow_mut().drain_while_holding = true;
  }

  /// The capacity the backend was initialized with.
  pub fn capacity(&self) -> Option<usize> {
    self.0.borrow().init
  }

  /// Every id ever pushed, in push order.
  pub fn pushed(&self) -> Vec<u64> {
    self.0.borrow().pushed.clone()
  }

  /// Ids pushed and not yet completed, in push order.
  pub fn in_flight(&self) -> Vec<u64> {
    self.0.borrow().in_flight.iter().map(|(id, _)| *id).collect()
  }

  /// The operation `id` was pushed with, while it is in flight.
  pub fn op(&self, id: u64) -> Option<Op> {
    let script = self.0.borrow();
    script.in_flight.iter().find(|(i, _)| *i == id).map(|(_, op)| op.clone())
  }

  /// Every id the driver asked to cancel, in order.
  pub fn cancelled(&self) -> Vec<u64> {
    self.0.borrow().cancelled.clone()
  }

  /// Finishes in-flight operation `id` with `result`, reported by the next
  /// `wait()`.
  ///
  /// # Panics
  ///
  /// If `id` is not in flight: the test would be scripting a completion no
  /// real backend could produce.
  pub fn complete(&self, id: u64, result: isize) {
    let mut script = self.0.borrow_mut();
    let Some(pos) = script.in_flight.iter().position(|(i, _)| *i == id) else {
      panic!("ScriptedBackend::complete: operation {id} is not in flight");
    };
    let (_, op) = script.in_flight.remove(pos);
    let Some(ticket) = script.tickets.remove(&id) else {
      unreachable!("ScriptedBackend: operation {id} is in flight unticketed");
    };
    if !script.no_poison {
      // SAFETY: the operation is still in flight, so the driver keeps the
      // memory it points to valid until it sees this completion.
      unsafe { touch(&op) };
    }
    // SAFETY: no kernel or thread ever sees the addresses. Every byte the
    // operation could receive was just written, so any byte count the test
    // scripts is one that was written; `without_poison` hands that claim to
    // the test.
    script.ready.push_back(unsafe { ticket.complete(result) });
  }

  /// Finishes every in-flight operation with `result`, in push order.
  pub fn complete_all(&self, result: isize) {
    for id in self.in_flight() {
      self.complete(id, result);
    }
  }
}

impl IoBackend for ScriptedBackend {
  fn init(&mut self, cap: usize) -> io::Result<()> {
    self.0.borrow_mut().init = Some(cap);
    Ok(())
  }

  fn resource_close(&self) -> ResourceClose {
    ResourceClose::noop()
  }

  fn push(&mut self, op: crate::backend::InFlight<'_>, _step_bump: &mut Bump) {
    let (op, ticket) = op.into_parts();
    let id = ticket.id();
    let mut script = self.0.borrow_mut();
    script.pushed.push(id);
    script.in_flight.push((id, op));
    script.tickets.insert(id, ticket);
  }

  fn flush(&mut self) -> io::Result<()> {
    let auto = self.0.borrow().auto;
    if let Some(result) = auto {
      self.complete_all(result);
    }
    Ok(())
  }

  fn cancel(&mut self, id: u64) {
    self.0.borrow_mut().cancelled.push(id);
  }

  fn drain(&mut self) -> bool {
    let mut script = self.0.borrow_mut();
    if script.stuck {
      return false;
    }
    if !script.drain_while_holding {
      for (_, op) in std::mem::take(&mut script.in_flight) {
        if !script.no_poison {
          // SAFETY: a drained operation may still be finishing, so the driver
          // keeps its memory valid until its ticket is given up.
          unsafe { touch(&op) };
        }
      }
      for (_, ticket) in script.tickets.drain() {
        // SAFETY: no kernel or thread ever sees the addresses.
        unsafe { ticket.release() };
      }
      script.ready.clear();
    }
    // True means this backend no longer stores a caller buffer address.
    // The flag above skips that forgetting, so the promise fails closed.
    assert!(
      !script.in_flight.iter().any(|(_, op)| holds_caller_buffer(op)),
      "backend reports drained while it still stores a caller buffer address"
    );
    true
  }

  fn wait(
    &mut self,
    _timeout: Option<Duration>,
    completed: &mut Vec<OpCompleted>,
  ) -> io::Result<()> {
    completed.clear();
    completed.extend(self.0.borrow_mut().ready.drain(..));
    Ok(())
  }
}

/// Whether `op` stores an address of a buffer the caller owns.
///
/// A read is the operation the drain promise is checked with. A count of
/// zero names no buffer.
fn holds_caller_buffer(op: &Op) -> bool {
  match op {
    Op::Read { iov_count, .. } => *iov_count > 0,
    _ => false,
  }
}

/// Reads every byte an operation sends and overwrites every byte it receives
/// into with [`POISON`], as a kernel performing it could.
///
/// Only plain byte buffers are touched: typed outputs such as a `FileStat`
/// would need valid values written.
///
/// # Safety
///
/// The memory `op` points to must be valid as the [`IoBackend`] contract
/// requires for an operation in flight.
unsafe fn touch(op: &Op) {
  match op {
    Op::Read { iovecs, iov_count, .. } => {
      // SAFETY: the caller guarantees the descriptor array is valid.
      // A shared borrow inspects each descriptor. It does not copy one.
      let bufs =
        unsafe { std::slice::from_raw_parts(iovecs.as_ptr(), *iov_count) };
      for buf in bufs {
        // SAFETY: the caller guarantees the buffer is valid for writes.
        unsafe { ptr::write_bytes(buf.ptr, POISON, buf.len) };
      }
    }
    Op::Write { iovecs, iov_count, .. } => {
      // SAFETY: the caller guarantees the descriptor array is valid.
      let bufs = unsafe { raw_parts(iovecs.as_ptr(), *iov_count) };
      for buf in bufs {
        // SAFETY: the caller guarantees the buffer is valid for reads.
        unsafe { read_bytes(buf.ptr, buf.len) };
      }
    }
    Op::Recv { msg, .. } => {
      // SAFETY: the caller guarantees the descriptor array is valid.
      let bufs = unsafe {
        raw_parts::<MsgBufMut>(msg.bufs.as_ptr(), msg.buf_count.get())
      };
      for buf in bufs {
        // SAFETY: the caller guarantees the buffer is valid for writes.
        unsafe { ptr::write_bytes(buf.ptr.as_ptr(), POISON, buf.len) };
      }
    }
    Op::Send { msg, .. } => {
      // SAFETY: the caller guarantees the descriptor array is valid.
      let bufs =
        unsafe { raw_parts::<MsgBuf>(msg.bufs.as_ptr(), msg.buf_count.get()) };
      for buf in bufs {
        // SAFETY: the caller guarantees the buffer is valid for reads.
        unsafe { read_bytes(buf.ptr.as_ptr(), buf.len) };
      }
    }
    Op::ReadlinkAt { buf, buf_len, .. } => {
      // SAFETY: the caller guarantees the buffer is valid for writes.
      unsafe { ptr::write_bytes(buf.as_ptr(), POISON, *buf_len) };
    }
    Op::ReadDir { raw_buf, raw_cap, .. } => {
      // SAFETY: the caller guarantees the buffer is valid for writes.
      unsafe { ptr::write_bytes(raw_buf.as_ptr(), POISON, *raw_cap) };
    }
    _ => {}
  }
}

/// Copies out `count` descriptors starting at `first`.
///
/// # Safety
///
/// `first` must be valid for reads of `count` values.
unsafe fn raw_parts<T: Copy>(first: *const T, count: usize) -> Vec<T> {
  // SAFETY: in bounds per the caller's guarantee.
  (0..count).map(|i| unsafe { first.add(i).read() }).collect()
}

/// Reads `len` bytes without creating a reference to them.
///
/// # Safety
///
/// `ptr` must be valid for reads of `len` bytes.
unsafe fn read_bytes(ptr: *const u8, len: usize) {
  for i in 0..len {
    // SAFETY: in bounds per the caller's guarantee.
    black_box(unsafe { ptr.add(i).read() });
  }
}

/// Wraps a backend and panics when either side breaks the [`IoBackend`]
/// contract.
///
/// It checks that the driver:
/// - calls `init` exactly once, before anything else
/// - never pushes an id that is still in flight
/// - never has more operations in flight than the initialized capacity
/// - never drops the backend with operations in flight unless `drain`
///   succeeded
///
/// and that the backend:
/// - completes only ids that are in flight, each exactly once
///
/// An operation is in flight from its `push` until its completion is
/// returned by `wait`, or until a successful `drain`.
pub struct Checked<B> {
  inner: B,
  cap: Option<usize>,
  in_flight: HashSet<u64>,
}

impl<B: IoBackend> Checked<B> {
  /// Wraps `inner`.
  pub fn new(inner: B) -> Self {
    Self { inner, cap: None, in_flight: HashSet::new() }
  }

  /// The wrapped backend.
  pub fn inner(&self) -> &B {
    &self.inner
  }

  /// Number of operations in flight.
  pub fn in_flight(&self) -> usize {
    self.in_flight.len()
  }

  fn cap(&self, call: &str) -> usize {
    self
      .cap
      .unwrap_or_else(|| panic!("contract: `{call}` called before `init`"))
  }
}

impl<B: IoBackend> IoBackend for Checked<B> {
  fn init(&mut self, cap: usize) -> io::Result<()> {
    assert!(self.cap.is_none(), "contract: `init` called twice");
    self.inner.init(cap)?;
    self.cap = Some(cap);
    Ok(())
  }

  fn resource_close(&self) -> ResourceClose {
    self.inner.resource_close()
  }

  fn push(&mut self, op: crate::backend::InFlight<'_>, step_bump: &mut Bump) {
    let cap = self.cap("push");
    let id = op.id();
    assert!(
      self.in_flight.len() < cap,
      "contract: pushed operation {id} with {cap} already in flight (capacity \
       {cap})"
    );
    assert!(
      self.in_flight.insert(id),
      "contract: pushed operation {id} while it is still in flight"
    );
    self.inner.push(op, step_bump);
  }

  fn flush(&mut self) -> io::Result<()> {
    self.cap("flush");
    self.inner.flush()
  }

  fn cancel(&mut self, id: u64) {
    self.cap("cancel");
    self.inner.cancel(id);
  }

  fn drain(&mut self) -> bool {
    let drained = self.inner.drain();
    if drained {
      self.in_flight.clear();
    }
    drained
  }

  fn wait(
    &mut self,
    timeout: Option<Duration>,
    completed: &mut Vec<OpCompleted>,
  ) -> io::Result<()> {
    self.cap("wait");
    self.inner.wait(timeout, completed)?;
    for done in completed.iter() {
      let id = done.registration_id();
      assert!(
        self.in_flight.remove(&id),
        "contract: backend completed operation {id}, which is not in flight \
         (unknown, or already completed)"
      );
    }
    Ok(())
  }
}

impl<B> Drop for Checked<B> {
  fn drop(&mut self) {
    if !std::thread::panicking() && !self.in_flight.is_empty() {
      panic!(
        "contract: backend dropped with {} operations in flight and no \
         successful `drain`",
        self.in_flight.len()
      );
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::backend::op::BorrowedOp;

  fn push_op(backend: &mut impl IoBackend, id: u64, op: Op) {
    // SAFETY: every buffer these tests submit stays alive until the backend
    // completes or drains the operation.
    unsafe {
      backend.push(
        crate::backend::InFlight::attest(id, BorrowedOp::from_op(op)),
        &mut Bump::new(),
      )
    };
  }
  use crate::backend::op::RawBuf;

  fn init(cap: usize) -> (ScriptedBackend, Checked<ScriptedBackend>) {
    let backend = ScriptedBackend::new();
    let mut checked = Checked::new(backend.clone());
    checked.init(cap).unwrap();
    (backend, checked)
  }

  fn wait(backend: &mut impl IoBackend) -> Vec<u64> {
    let mut completed = Vec::new();
    backend.wait(Some(Duration::ZERO), &mut completed).unwrap();
    completed.iter().map(|c| c.registration_id()).collect()
  }

  #[test]
  fn dropping_the_backend_gives_up_the_operations_it_still_holds() {
    let keep = crate::backend::Keep::new();
    let mut backend = ScriptedBackend::new();
    backend.init(4).unwrap();
    // SAFETY: a nop carries no addresses.
    let op = unsafe { BorrowedOp::from_op(Op::Nop) };
    backend.push(
      crate::backend::InFlight::from_model(1, op, keep.clone()),
      &mut Bump::new(),
    );
    assert!(keep.held_elsewhere(), "the backend holds the operation");

    drop(backend);

    assert!(!keep.held_elsewhere(), "the operation was given up");
  }

  #[test]
  fn holds_operations_until_completed() {
    let (backend, mut checked) = init(4);
    push_op(&mut checked, 1, Op::Nop);
    push_op(&mut checked, 2, Op::Nop);
    checked.flush().unwrap();
    assert!(wait(&mut checked).is_empty());

    backend.complete(2, 0);
    assert_eq!(wait(&mut checked), vec![2]);
    assert_eq!(backend.in_flight(), vec![1]);
    assert_eq!(checked.in_flight(), 1);

    backend.complete(1, -1);
    assert_eq!(wait(&mut checked), vec![1]);
    assert_eq!(backend.pushed(), vec![1, 2]);
  }

  #[test]
  fn auto_completes_on_flush_in_push_order() {
    let mut backend = Checked::new(ScriptedBackend::auto(7));
    backend.init(4).unwrap();
    for id in [3, 1, 2] {
      push_op(&mut backend, id, Op::Nop);
    }
    assert!(wait(&mut backend).is_empty(), "nothing before the flush");
    backend.flush().unwrap();
    let mut completed = Vec::new();
    backend.wait(Some(Duration::ZERO), &mut completed).unwrap();
    assert_eq!(
      completed
        .iter()
        .map(|c| (c.registration_id(), c.result()))
        .collect::<Vec<_>>(),
      vec![(3, 7), (1, 7), (2, 7)]
    );
  }

  #[test]
  fn poisons_received_bytes_and_reads_sent_bytes() {
    let (backend, mut checked) = init(2);
    let mut into = [0u8; 8];
    let read_buf = [crate::backend::op::RawBufMut::from_mut(&mut into)];
    let from = *b"payload!";
    let write_buf = [RawBuf::new(&from)];
    let fd = crate::api::resource::Resource::stdout();
    let read = Op::Read {
      fd: fd.clone(),
      iovecs: ptr::NonNull::from(&read_buf[0]).cast(),
      iov_count: 1,
      offset: None,
      flags: Default::default(),
    };
    let write = Op::Write {
      fd,
      iovecs: ptr::NonNull::from(&write_buf[0]).cast(),
      iov_count: 1,
      offset: None,
      flags: Default::default(),
    };

    push_op(&mut checked, 1, read);
    push_op(&mut checked, 2, write);
    backend.complete_all(8);
    assert_eq!(into, [POISON; 8]);
    assert_eq!(&from, b"payload!", "sent bytes are only read");
    assert_eq!(wait(&mut checked), vec![1, 2]);
  }

  /// A read of one byte into `into`.
  fn read_into(
    into: &mut u8,
    iovec: &mut crate::backend::op::RawBufMut<'_>,
  ) -> Op {
    // SAFETY: `into` is one live byte, and the caller keeps it for the read.
    *iovec = unsafe { crate::backend::op::RawBufMut::from_raw_parts(into, 1) };
    Op::Read {
      fd: crate::api::resource::Resource::stdout(),
      iovecs: ptr::NonNull::from(&*iovec).cast(),
      iov_count: 1,
      offset: None,
      flags: Default::default(),
    }
  }

  #[test]
  fn records_its_capacity() {
    let (backend, _checked) = init(4);
    assert_eq!(backend.capacity(), Some(4));
    assert_eq!(ScriptedBackend::new().capacity(), None);
  }

  #[test]
  fn drain_poisons_unless_told_not_to() {
    for poison in [true, false] {
      let backend = ScriptedBackend::new();
      let mut backend = if poison {
        backend
      } else {
        // SAFETY: the operation below is drained, never completed.
        unsafe { backend.without_poison() }
      };
      backend.init(1).unwrap();
      let (mut into, mut iovec) = (0u8, crate::backend::op::RawBufMut::empty());
      push_op(&mut backend, 1, read_into(&mut into, &mut iovec));
      assert!(backend.drain());
      assert_eq!(into, if poison { POISON } else { 0 }, "poison: {poison}");
    }
  }

  #[test]
  fn without_poison_completes_without_touching_buffers() {
    // SAFETY: the one read completed below points at an initialized byte.
    let backend = unsafe { ScriptedBackend::new().without_poison() };
    let mut driver = backend.clone();
    driver.init(1).unwrap();
    let (mut into, mut iovec) = (0u8, crate::backend::op::RawBufMut::empty());
    push_op(&mut driver, 1, read_into(&mut into, &mut iovec));
    backend.complete(1, 1);
    assert_eq!(into, 0);
    assert_eq!(wait(&mut driver), vec![1]);
  }

  #[test]
  #[should_panic(expected = "still stores a caller buffer address")]
  fn drain_fails_if_it_reports_drained_while_a_caller_buffer_is_stored() {
    let mut backend = ScriptedBackend::new();
    backend.drain_while_holding();
    backend.init(1).unwrap();
    let (mut into, mut iovec) = (0u8, crate::backend::op::RawBufMut::empty());
    push_op(&mut backend.clone(), 1, read_into(&mut into, &mut iovec));
    let _ = backend.drain();
  }

  #[test]
  fn drain_allows_a_read_that_names_no_buffer() {
    let mut backend = ScriptedBackend::new();
    backend.drain_while_holding();
    backend.init(1).unwrap();
    let read = Op::Read {
      fd: crate::api::resource::Resource::stdout(),
      iovecs: ptr::NonNull::dangling(),
      iov_count: 0,
      offset: None,
      flags: Default::default(),
    };
    push_op(&mut backend, 1, read);
    assert!(backend.drain());
  }

  #[test]
  fn records_cancels_and_drains() {
    let (backend, mut checked) = init(2);
    push_op(&mut checked, 1, Op::Nop);
    checked.cancel(1);
    checked.cancel(99);
    assert_eq!(backend.cancelled(), vec![1, 99]);

    backend.set_stuck(true);
    assert!(!checked.drain());
    backend.set_stuck(false);
    assert!(checked.drain());
    assert!(backend.in_flight().is_empty());
    assert_eq!(checked.in_flight(), 0);
  }

  #[test]
  #[should_panic(expected = "is not in flight")]
  fn completing_an_unknown_operation_panics() {
    ScriptedBackend::new().complete(1, 0);
  }

  #[test]
  #[should_panic(expected = "before `init`")]
  fn push_before_init_breaks_the_contract() {
    push_op(&mut Checked::new(ScriptedBackend::new()), 1, Op::Nop);
  }

  #[test]
  #[should_panic(expected = "`init` called twice")]
  fn second_init_breaks_the_contract() {
    let (_, mut checked) = init(1);
    let _ = checked.init(1);
  }

  #[test]
  #[should_panic(expected = "still in flight")]
  fn pushing_an_in_flight_id_breaks_the_contract() {
    let (_, mut checked) = init(2);
    push_op(&mut checked, 1, Op::Nop);
    push_op(&mut checked, 1, Op::Nop);
  }

  #[test]
  #[should_panic(expected = "already in flight (capacity 1)")]
  fn pushing_over_capacity_breaks_the_contract() {
    let (_, mut checked) = init(1);
    push_op(&mut checked, 1, Op::Nop);
    push_op(&mut checked, 2, Op::Nop);
  }

  #[test]
  #[should_panic(expected = "which is not in flight")]
  fn completing_twice_breaks_the_contract() {
    struct Twice(Option<u64>);
    impl IoBackend for Twice {
      fn init(&mut self, _: usize) -> io::Result<()> {
        Ok(())
      }
      fn push(&mut self, op: crate::backend::InFlight<'_>, _: &mut Bump) {
        self.0 = Some(op.id());
      }
      fn flush(&mut self) -> io::Result<()> {
        Ok(())
      }
      fn wait(
        &mut self,
        _: Option<Duration>,
        completed: &mut Vec<OpCompleted>,
      ) -> io::Result<()> {
        completed.clear();
        // Reported without a ticket, which only a test can do.
        completed.extend(self.0.map(|id| OpCompleted::forged(id, 0)));
        Ok(())
      }

      fn resource_close(&self) -> crate::api::resource::ResourceClose {
        crate::api::resource::ResourceClose::native()
      }

      // Its operations finish on their own: there is nothing to stop.
      fn cancel(&mut self, _id: u64) {}

      // Nothing outside this type ever holds an address.
      fn drain(&mut self) -> bool {
        true
      }
    }

    let mut checked = Checked::new(Twice(None));
    checked.init(1).unwrap();
    push_op(&mut checked, 1, Op::Nop);
    wait(&mut checked);
    wait(&mut checked);
  }

  #[test]
  #[should_panic(expected = "no successful `drain`")]
  fn dropping_with_operations_in_flight_breaks_the_contract() {
    let (_, mut checked) = init(1);
    push_op(&mut checked, 1, Op::Nop);
  }
}
