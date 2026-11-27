//! Backends for testing lio's bookkeeping without doing any I/O.
//!
//! - [`ScriptedBackend`] performs nothing on its own: the test decides which
//!   operations complete, in which order and with which result. By default it
//!   also touches the memory of every operation when it completes or is
//!   stopped, the way the kernel would, so a buffer freed or reused too early
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
use std::collections::{HashSet, VecDeque};
use std::hint::black_box;
use std::io;
use std::ptr;
use std::rc::Rc;
use std::time::Duration;

use bumpalo::Bump;

use crate::api::resource::ResourceClose;
use crate::backend::op::{MsgBuf, MsgBufMut, Op};
use crate::backend::{Done, IoBackend, OpKey, Queued, Started};

/// The byte [`ScriptedBackend`] writes into the buffers it fills.
pub const POISON: u8 = 0xA5;

/// The result [`ScriptedBackend`] reports for an operation it stopped.
pub const CANCELLED: isize = -125;
// A stopped operation reports an error.
const _: () = assert!(CANCELLED < 0);

#[derive(Default)]
struct Script {
  init: Option<usize>,
  /// Every id ever pushed, in push order.
  pushed: Vec<u64>,
  /// Started and not yet completed, in push order.
  in_flight: Vec<(u64, OpKey, Op)>,
  cancelled: Vec<u64>,
  /// Completed, waiting for the next `wait()`: the key, the result, and
  /// whether the operation was stopped.
  ready: VecDeque<(OpKey, isize, bool)>,
  auto: Option<isize>,
  stuck: bool,
  no_poison: bool,
}

/// A backend that only does what the test tells it to.
///
/// Clones share their state: hand one clone to [`Lio`](crate::Lio) and keep
/// another to drive it. Pushed operations stay in flight until
/// [`complete`](Self::complete) is called for them; the completion is then
/// reported by the next `wait()`.
///
/// The driver and the test share a thread, so nothing can be scripted while
/// the driver blocks. A `wait()` that is allowed to block (any timeout but
/// zero) and has nothing to report is therefore when the scripted kernel
/// acts on [`cancel`](IoBackend::cancel): every operation the driver asked
/// to stop is reported with [`CANCELLED`]. Until then a cancel is only
/// recorded, and the test may still complete the operation itself.
/// [`set_stuck`](Self::set_stuck) makes the kernel ignore cancels. `wait()`
/// never blocks otherwise.
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

  /// A backend that completes every operation with `result`, reported by
  /// the wait after it was pushed, in push order.
  pub fn auto(result: isize) -> Self {
    let backend = Self::new();
    backend.0.borrow_mut().auto = Some(result);
    backend
  }

  /// Stops touching operation memory when an operation completes or is
  /// stopped.
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

  /// Makes the backend ignore [`cancel`](IoBackend::cancel): cancelled
  /// operations stay in flight, so a driver dropped with one has to leak
  /// its memory. A `wait()` with a timeout then takes that long, as a
  /// kernel with nothing to report would.
  pub fn set_stuck(&self, stuck: bool) {
    self.0.borrow_mut().stuck = stuck;
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
    self.0.borrow().in_flight.iter().map(|(id, ..)| *id).collect()
  }

  /// The operation `id` was pushed with, while it is in flight.
  pub fn op(&self, id: u64) -> Option<Op> {
    let script = self.0.borrow();
    script.in_flight.iter().find(|(i, ..)| *i == id).map(|(.., op)| op.clone())
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
    let Some(pos) = script.in_flight.iter().position(|(i, ..)| *i == id) else {
      panic!("ScriptedBackend::complete: operation {id} is not in flight");
    };
    let (_, key, op) = script.in_flight.remove(pos);
    if !script.no_poison {
      // SAFETY: the operation is still in flight, so the driver keeps the
      // memory it points to valid until it sees this completion.
      unsafe { touch(&op) };
    }
    script.ready.push_back((key, result, false));
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

  fn start(
    &mut self,
    op: Queued<'_>,
    _step_bump: &mut Bump,
    _done: &mut Done<'_>,
  ) -> Started {
    // The scripted kernel has the operation from here on.
    let id = op.id();
    let (started, key, op) = op.begin();
    let auto = {
      let mut script = self.0.borrow_mut();
      script.pushed.push(id);
      script.in_flight.push((id, key, std::mem::replace(op, Op::Nop)));
      script.auto
    };
    if let Some(result) = auto {
      self.complete(id, result);
    }
    started
  }

  fn cancel(&mut self, key: OpKey, _done: &mut Done<'_>) {
    let mut script = self.0.borrow_mut();
    let id = script.in_flight.iter().find(|(_, k, _)| *k == key).map(|e| e.0);
    // One completed by the test already reports that completion.
    script.cancelled.extend(id);
  }

  fn wait(
    &mut self,
    timeout: Option<Duration>,
    done: &mut Done<'_>,
  ) -> io::Result<()> {
    let mut script = self.0.borrow_mut();
    let idle = script.ready.is_empty() && done.waiting() == 0;
    if idle && timeout != Some(Duration::ZERO) {
      if script.stuck {
        drop(script);
        std::thread::sleep(timeout.unwrap_or(Duration::ZERO));
        return Ok(());
      }
      script.stop_cancelled();
    }
    for (key, result, stopped) in script.ready.drain(..) {
      // SAFETY: no kernel or thread ever sees the addresses, and the
      // operation left `in_flight` when this report was queued. Every byte
      // the operation could receive was written then, so any byte count the
      // test scripts is one that was written; `without_poison` hands that
      // claim to the test. A stopped operation reports `CANCELLED`, which
      // is negative.
      unsafe {
        if stopped {
          done.cancelled(key, result)
        } else {
          done.complete(key, result)
        }
      }
    }
    Ok(())
  }
}

impl Script {
  /// Reports every in-flight operation the driver asked to cancel.
  fn stop_cancelled(&mut self) {
    let cancelled = &self.cancelled;
    let (stopped, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut self.in_flight)
      .into_iter()
      .partition(|(id, ..)| cancelled.contains(id));
    self.in_flight = kept;
    for (_, key, op) in stopped {
      if !self.no_poison {
        // SAFETY: a stopped operation may have been finishing, so the driver
        // keeps its memory valid until it sees this completion.
        unsafe { touch(&op) };
      }
      self.ready.push_back((key, CANCELLED, true));
    }
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

/// Wraps a backend and panics when the driver breaks the [`IoBackend`]
/// contract.
///
/// It checks that the driver:
/// - calls `init` exactly once, before anything else
/// - never drops the backend with operations in flight
///
/// An operation is in flight from the `start` that begins it until the
/// backend reports it. What a backend could get wrong about reporting (an
/// unknown operation, one reported twice) the table it reports to rules
/// out, and so does [`Tracked`](crate::backend::Tracked) for an id pushed
/// twice or past the capacity.
pub struct Checked<B> {
  inner: B,
  cap: Option<usize>,
  /// The ids of the operations that were begun and not reported.
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

  /// Runs one call of the wrapped backend, and takes what it reported out
  /// of the operations in flight.
  fn tracking<R>(
    &mut self,
    done: &mut Done<'_>,
    call: impl FnOnce(&mut B, &mut Done<'_>) -> R,
  ) -> R {
    let before = done.waiting();
    let result = call(&mut self.inner, done);
    for reported in &done.reported()[before..] {
      self.in_flight.remove(&reported.registration_id());
    }
    result
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

  fn start(
    &mut self,
    op: Queued<'_>,
    step_bump: &mut Bump,
    done: &mut Done<'_>,
  ) -> Started {
    self.cap("start");
    // In flight before the call: the backend may report it from there.
    let id = op.id();
    self.in_flight.insert(id);
    let started =
      self.tracking(done, |inner, done| inner.start(op, step_bump, done));
    if !started.began() {
      self.in_flight.remove(&id);
    }
    started
  }

  fn cancel(&mut self, key: OpKey, done: &mut Done<'_>) {
    self.cap("cancel");
    self.tracking(done, |inner, done| inner.cancel(key, done));
  }

  fn wait(
    &mut self,
    timeout: Option<Duration>,
    done: &mut Done<'_>,
  ) -> io::Result<()> {
    self.cap("wait");
    self.tracking(done, |inner, done| inner.wait(timeout, done))
  }
}

impl<B> Drop for Checked<B> {
  fn drop(&mut self) {
    if !std::thread::panicking() && !self.in_flight.is_empty() {
      panic!(
        "contract: backend dropped with {} operations in flight",
        self.in_flight.len()
      );
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::backend::Tracked;
  use crate::backend::op::{BorrowedOp, RawBuf};

  type Driven = Tracked<Checked<ScriptedBackend>>;

  fn push_op<B: IoBackend>(io: &mut Tracked<B>, id: u64, op: Op) {
    // SAFETY: every buffer these tests submit stays alive until the backend
    // has reported the operation.
    unsafe { io.push(id, BorrowedOp::from_op(op)) };
  }

  fn init(cap: usize) -> (ScriptedBackend, Driven) {
    let backend = ScriptedBackend::new();
    let mut io = Tracked::new(Checked::new(backend.clone()));
    io.init(cap).unwrap();
    (backend, io)
  }

  fn wait<B: IoBackend>(io: &mut Tracked<B>) -> Vec<u64> {
    let mut completed = Vec::new();
    io.wait(Some(Duration::ZERO), &mut completed).unwrap();
    completed.iter().map(|c| c.registration_id()).collect()
  }

  #[test]
  fn dropping_the_backend_gives_up_the_operations_it_still_holds() {
    let keep = crate::backend::Keep::new();
    let mut io = Tracked::new(ScriptedBackend::new());
    io.init(4).unwrap();
    // SAFETY: a nop carries no addresses.
    let op = unsafe { BorrowedOp::from_op(Op::Nop) };
    io.push_model(1, op, keep.clone());
    assert!(keep.held_elsewhere(), "the backend has the operation");

    drop(io);

    assert!(!keep.held_elsewhere(), "the operation was given up");
  }

  #[test]
  fn holds_operations_until_completed() {
    let (backend, mut io) = init(4);
    push_op(&mut io, 1, Op::Nop);
    push_op(&mut io, 2, Op::Nop);
    assert_eq!(backend.pushed(), vec![1, 2], "started as they were pushed");
    assert!(wait(&mut io).is_empty());

    backend.complete(2, 0);
    assert_eq!(wait(&mut io), vec![2]);
    assert_eq!(backend.in_flight(), vec![1]);
    assert_eq!(io.backend().in_flight(), 1);

    backend.complete(1, -1);
    assert_eq!(wait(&mut io), vec![1]);
    assert_eq!(backend.pushed(), vec![1, 2]);
    assert_eq!(io.backend().in_flight(), 0);
  }

  #[test]
  fn auto_completes_as_pushed_in_push_order() {
    let mut io = Tracked::new(Checked::new(ScriptedBackend::auto(7)));
    io.init(4).unwrap();
    for id in [3, 1, 2] {
      push_op(&mut io, id, Op::Nop);
    }
    let mut completed = Vec::new();
    io.wait(Some(Duration::ZERO), &mut completed).unwrap();
    assert_eq!(
      completed
        .iter()
        .map(|c| (c.registration_id(), c.result()))
        .collect::<Vec<_>>(),
      vec![(3, 7), (1, 7), (2, 7)]
    );
    assert_eq!(io.backend().in_flight(), 0, "each was counted out once");
  }

  #[test]
  fn poisons_received_bytes_and_reads_sent_bytes() {
    let (backend, mut io) = init(2);
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

    push_op(&mut io, 1, read);
    push_op(&mut io, 2, write);
    backend.complete_all(8);
    assert_eq!(into, [POISON; 8]);
    assert_eq!(&from, b"payload!", "sent bytes are only read");
    assert_eq!(wait(&mut io), vec![1, 2]);
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
    let (backend, _io) = init(4);
    assert_eq!(backend.capacity(), Some(4));
    assert_eq!(ScriptedBackend::new().capacity(), None);
  }

  #[test]
  fn stopping_poisons_unless_told_not_to() {
    for poison in [true, false] {
      let backend = ScriptedBackend::new();
      let backend = if poison {
        backend
      } else {
        // SAFETY: the operation below is stopped, never completed.
        unsafe { backend.without_poison() }
      };
      let mut io = Tracked::new(backend);
      io.init(1).unwrap();
      let (mut into, mut iovec) = (0u8, crate::backend::op::RawBufMut::empty());
      push_op(&mut io, 1, read_into(&mut into, &mut iovec));
      io.cancel(1);
      let mut completed = Vec::new();
      io.wait(None, &mut completed).unwrap();
      assert!(completed[0].is_cancelled());
      assert_eq!(completed[0].result(), CANCELLED);
      assert_eq!(into, if poison { POISON } else { 0 }, "poison: {poison}");
    }
  }

  #[test]
  fn without_poison_completes_without_touching_buffers() {
    // SAFETY: the one read completed below points at an initialized byte.
    let backend = unsafe { ScriptedBackend::new().without_poison() };
    let mut io = Tracked::new(backend.clone());
    io.init(1).unwrap();
    let (mut into, mut iovec) = (0u8, crate::backend::op::RawBufMut::empty());
    push_op(&mut io, 1, read_into(&mut into, &mut iovec));
    backend.complete(1, 1);
    assert_eq!(into, 0);
    assert_eq!(wait(&mut io), vec![1]);
  }

  #[test]
  fn a_cancel_is_recorded_and_acted_on_when_the_driver_blocks() {
    let (backend, mut io) = init(2);
    push_op(&mut io, 1, Op::Nop);
    push_op(&mut io, 2, Op::Nop);
    io.cancel(1);
    io.cancel(99);
    assert_eq!(
      backend.cancelled(),
      vec![1],
      "an unknown id stops at the table"
    );
    assert!(wait(&mut io).is_empty(), "a poll leaves it in flight");
    assert_eq!(backend.in_flight(), vec![1, 2]);

    backend.set_stuck(true);
    let mut completed = Vec::new();
    io.wait(Some(Duration::from_millis(1)), &mut completed).unwrap();
    assert!(completed.is_empty(), "a stuck backend ignores the cancel");

    backend.set_stuck(false);
    io.wait(None, &mut completed).unwrap();
    assert_eq!(completed.len(), 1);
    assert!(completed[0].is_cancelled());
    assert_eq!(backend.in_flight(), vec![2], "only what was cancelled");
    backend.complete(2, 0);
    assert_eq!(wait(&mut io), vec![2]);
  }

  #[test]
  fn a_stuck_backend_still_reports_what_the_test_completed() {
    let (backend, mut io) = init(2);
    push_op(&mut io, 1, Op::Nop);
    push_op(&mut io, 2, Op::Nop);
    io.cancel(2);
    backend.set_stuck(true);
    backend.complete(1, 0);
    let mut completed = Vec::new();
    io.wait(Some(Duration::from_millis(1)), &mut completed).unwrap();
    let ids: Vec<u64> = completed.iter().map(|c| c.registration_id()).collect();
    assert_eq!(ids, [1], "the completion is not held back, the cancel is");
    assert_eq!(backend.in_flight(), vec![2]);
    backend.complete(2, 0);
    assert_eq!(wait(&mut io), vec![2]);
  }

  #[test]
  #[should_panic(expected = "is not in flight")]
  fn completing_an_unknown_operation_panics() {
    ScriptedBackend::new().complete(1, 0);
  }

  #[test]
  #[should_panic(expected = "before `init`")]
  fn start_before_init_breaks_the_contract() {
    // The table is initialized here, and the backend under `Checked` is not.
    let mut io = Tracked::new(Uninit(Checked::new(ScriptedBackend::new())));
    io.init(1).unwrap();
    push_op(&mut io, 1, Op::Nop);
  }

  /// Forwards everything but `init`.
  struct Uninit<B>(B);

  impl<B: IoBackend> IoBackend for Uninit<B> {
    fn init(&mut self, _: usize) -> io::Result<()> {
      Ok(())
    }
    fn resource_close(&self) -> ResourceClose {
      self.0.resource_close()
    }
    fn start(
      &mut self,
      op: Queued<'_>,
      step_bump: &mut Bump,
      done: &mut Done<'_>,
    ) -> Started {
      self.0.start(op, step_bump, done)
    }
    fn cancel(&mut self, key: OpKey, done: &mut Done<'_>) {
      self.0.cancel(key, done)
    }
    fn wait(
      &mut self,
      timeout: Option<Duration>,
      done: &mut Done<'_>,
    ) -> io::Result<()> {
      self.0.wait(timeout, done)
    }
  }

  #[test]
  #[should_panic(expected = "`init` called twice")]
  fn second_init_breaks_the_contract() {
    let (_, mut io) = init(1);
    let _ = io.backend_mut().init(1);
  }

  #[test]
  #[should_panic(expected = "still in flight")]
  fn pushing_an_in_flight_id_is_refused_by_the_table() {
    let (_, mut io) = init(2);
    push_op(&mut io, 1, Op::Nop);
    push_op(&mut io, 1, Op::Nop);
  }

  #[test]
  #[should_panic(expected = "capacity exceeded")]
  fn pushing_over_capacity_is_refused_by_the_table() {
    let (_, mut io) = init(1);
    push_op(&mut io, 1, Op::Nop);
    push_op(&mut io, 2, Op::Nop);
  }

  /// Reports the one operation it is given at every chance it gets.
  struct Eager(Option<OpKey>);

  impl IoBackend for Eager {
    fn init(&mut self, _: usize) -> io::Result<()> {
      Ok(())
    }
    fn resource_close(&self) -> ResourceClose {
      ResourceClose::noop()
    }
    fn start(
      &mut self,
      op: Queued<'_>,
      _: &mut Bump,
      _: &mut Done<'_>,
    ) -> Started {
      let (started, key, _) = op.begin();
      self.0 = Some(key);
      started
    }
    // Its operations finish on their own: there is nothing to stop.
    fn cancel(&mut self, _: OpKey, _: &mut Done<'_>) {}
    fn wait(
      &mut self,
      _: Option<Duration>,
      done: &mut Done<'_>,
    ) -> io::Result<()> {
      if let Some(key) = self.0 {
        // SAFETY: a nop has no addresses, and 0 is what a nop does.
        unsafe { done.complete(key, 0) };
      }
      Ok(())
    }
  }

  #[test]
  fn a_second_report_of_an_operation_is_ignored() {
    let mut io = Tracked::new(Checked::new(Eager(None)));
    io.init(1).unwrap();
    push_op(&mut io, 1, Op::Nop);
    assert_eq!(wait(&mut io), vec![1]);
    assert!(wait(&mut io).is_empty(), "reported once");

    // The slot's next operation is not the one the stale key names.
    push_op(&mut io, 1, Op::Nop);
    assert_eq!(io.in_flight(), 1);
    assert_eq!(wait(&mut io), vec![1]);
    assert!(wait(&mut io).is_empty(), "reported once");
  }

  #[test]
  fn a_refused_operation_is_reported_failed() {
    struct Refusing;
    impl IoBackend for Refusing {
      fn init(&mut self, _: usize) -> io::Result<()> {
        Ok(())
      }
      fn resource_close(&self) -> ResourceClose {
        ResourceClose::noop()
      }
      fn start(
        &mut self,
        op: Queued<'_>,
        _: &mut Bump,
        _: &mut Done<'_>,
      ) -> Started {
        op.refuse(-22)
      }
      fn cancel(&mut self, _: OpKey, _: &mut Done<'_>) {}
      fn wait(
        &mut self,
        _: Option<Duration>,
        _: &mut Done<'_>,
      ) -> io::Result<()> {
        Ok(())
      }
    }

    let mut io = Tracked::new(Checked::new(Refusing));
    io.init(1).unwrap();
    push_op(&mut io, 1, Op::Nop);
    let mut completed = Vec::new();
    io.wait(Some(Duration::ZERO), &mut completed).unwrap();
    assert_eq!(completed.len(), 1);
    assert_eq!(completed[0].result(), -22);
    assert!(!completed[0].is_cancelled());
    assert_eq!(io.backend().in_flight(), 0);
  }

  #[test]
  #[should_panic(expected = "operations in flight")]
  fn dropping_with_operations_in_flight_breaks_the_contract() {
    let (_, mut io) = init(1);
    push_op(&mut io, 1, Op::Nop);
  }
}
