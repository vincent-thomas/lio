//! The table of operations a backend has been given, and the contract a
//! backend implements against it.
//!
//! [`Tracked`] owns every operation from the moment it is pushed until it is
//! reported. A backend sees an operation once, in [`IoBackend::start`], and
//! from then on knows it by an [`OpKey`]. Reporting it is a call on [`Done`],
//! which is the table as a backend sees it.

use std::io;
use std::time::Duration;

use bumpalo::Bump;

use crate::api::resource::ResourceClose;
use crate::backend::op::{BorrowedOp, Op};

/// The result of an operation that was cancelled before the backend was
/// given it.
///
/// The backend never saw the operation, so no platform produced this number
/// and it means nothing on its own: read [`OpCompleted::is_cancelled`].
pub const CANCELLED_BEFORE_START: isize = -125;
// A stopped operation reports an error.
const _: () = assert!(CANCELLED_BEFORE_START < 0);

/// One finished operation: what [`Tracked::wait`] reports.
///
/// Only the table makes one, from an operation it holds, so a backend cannot
/// report an operation it was not given, or report one twice. It carries the
/// operation's lease: the driver frees the operation's memory once it has
/// read this.
#[derive(Debug)]
pub struct OpCompleted {
  registration_id: u64,
  result: isize,
  cancelled: bool,
  _keep: Keep,
}

impl OpCompleted {
  /// Non-negative for success, a negative errno for an error.
  #[inline]
  pub fn result(&self) -> isize {
    self.result
  }

  #[inline]
  pub fn registration_id(&self) -> u64 {
    self.registration_id
  }

  /// Whether the operation was stopped by a cancel.
  ///
  /// `false` for one that finished first, even after a cancel was asked
  /// for: it reports its own result.
  #[inline]
  pub fn is_cancelled(&self) -> bool {
    self.cancelled
  }
}

/// One share of the lifetime of an operation's memory.
///
/// The driver holds one. The table holds another while the operation is
/// queued or with the backend, and the [`OpCompleted`] made for it holds
/// that share afterwards. The driver frees the model only when it holds the
/// last one.
///
/// Shares never leave the driver's thread: a backend that hands work to
/// another thread hands it an [`OpKey`].
#[derive(Clone, Debug)]
pub(crate) struct Keep {
  alive: std::rc::Rc<()>,
}

impl Keep {
  pub(crate) fn new() -> Self {
    Self { alive: std::rc::Rc::new(()) }
  }

  /// Another owner still has a copy.
  pub(crate) fn held_elsewhere(&self) -> bool {
    std::rc::Rc::strong_count(&self.alive) > 1
  }
}

/// The name a backend knows one started operation by.
///
/// It is plain data: it can be given to the kernel as `user_data` or sent to
/// a worker thread, and read back with [`from_u64`](Self::from_u64). Every
/// operation gets a new one, so a key that outlives its operation matches
/// nothing: reporting it is ignored.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct OpKey(u64);

impl OpKey {
  #[inline]
  fn new(slot: u32, generation: u32) -> Self {
    Self((generation as u64) << 32 | slot as u64)
  }

  /// A small index no two operations in flight at once share, which is
  /// what [`PerOp`] indexes by. It is below the capacity the backend was
  /// initialized with, except for the few slots that replace one whose ids
  /// ran out.
  #[inline]
  pub fn slot(self) -> usize {
    self.0 as u32 as usize
  }

  #[inline]
  fn generation(self) -> u32 {
    (self.0 >> 32) as u32
  }

  #[inline]
  pub fn as_u64(self) -> u64 {
    self.0
  }

  /// A key read back from where [`as_u64`](Self::as_u64) was stored. Any
  /// value is accepted: one that names no operation matches nothing.
  #[inline]
  pub fn from_u64(raw: u64) -> Self {
    Self(raw)
  }
}

/// One operation the backend is being given and has not started.
///
/// [`IoBackend::start`] gets it and must turn it into a [`Started`]:
/// [`begin`](Self::begin) it, or [`refuse`](Self::refuse) it. It cannot be
/// kept for later: it borrows the operation from the table, which holds
/// operations that are not started.
#[derive(Debug)]
pub struct Queued<'a> {
  key: OpKey,
  id: u64,
  /// In the table, where it stays unless the backend takes it.
  op: &'a mut Op,
}

/// Proof that [`IoBackend::start`] dealt with the operation it was given.
/// Only [`Queued::begin`] and [`Queued::refuse`] make one.
#[derive(Debug)]
#[must_use]
pub struct Started(Outcome);

#[derive(Debug)]
enum Outcome {
  Begun,
  Refused(isize),
}

impl Started {
  /// Whether the operation was begun, and is the backend's to report.
  #[inline]
  pub fn began(&self) -> bool {
    matches!(self.0, Outcome::Begun)
  }
}

impl<'a> Queued<'a> {
  /// The id the driver knows the operation by. A backend has no use for it
  /// beyond naming the operation to a person: it reports by [`OpKey`].
  #[inline]
  pub fn id(&self) -> u64 {
    self.id
  }

  /// The operation, to look at. Its addresses are not to be used before
  /// [`begin`](Self::begin).
  #[inline]
  pub fn as_op(&self) -> &Op {
    self.op
  }

  /// Begins the operation: the [`Op`] to act on, and the key to report it
  /// by.
  ///
  /// From here until the backend reports the key through [`Done`], the
  /// memory the operation points to stays valid, and the operation is the
  /// backend's to report: exactly once, from this call or any later one.
  ///
  /// The [`Op`] is lent for the rest of the call to [`IoBackend::start`],
  /// and dropped when that returns, with the resources it holds open. A
  /// backend that finishes the operation later and needs the [`Op`] until
  /// then takes it: `std::mem::replace(op, Op::Nop)`.
  #[inline]
  pub fn begin(self) -> (Started, OpKey, &'a mut Op) {
    (Started(Outcome::Begun), self.key, self.op)
  }

  /// Reports the operation as failed before it was begun: the backend would
  /// not or could not start it. `result` is the error.
  ///
  /// # Panics
  ///
  /// If `result` is not negative: an operation that never ran has no
  /// success to report.
  #[inline]
  pub fn refuse(self, result: isize) -> Started {
    assert!(result < 0, "an operation that never ran reports an error");
    Started(Outcome::Refused(result))
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
  Vacant,
  /// Pushed, and still in the table's backlog.
  Queued,
  /// Given to the backend, which has not reported it.
  Started,
}

impl Entry {
  fn vacant() -> Self {
    Self { generation: 0, state: State::Vacant, id: 0, keep: None }
  }
}

#[derive(Debug)]
struct Entry {
  /// Counts the operations this slot has held. Part of their keys.
  generation: u32,
  state: State,
  id: u64,
  /// The operation's lease, while the slot is not vacant.
  keep: Option<Keep>,
}

/// The table, as a backend sees it: where operations are reported.
///
/// A report names its operation by key. One for a key the table does not
/// hold as started (unknown, or reported already) is ignored.
#[derive(Debug)]
pub struct Done<'a> {
  entries: &'a mut [Entry],
  ready: &'a mut Vec<OpCompleted>,
  live: &'a mut usize,
}

impl Done<'_> {
  #[inline]
  fn report(&mut self, key: OpKey, result: isize, cancelled: bool) {
    let Some(entry) = self.entries.get_mut(key.slot()) else {
      return;
    };
    if entry.generation != key.generation() || entry.state != State::Started {
      return;
    }
    let Some(keep) = entry.keep.take() else {
      unreachable!("table: a started operation holds its lease");
    };
    entry.state = State::Vacant;
    *self.live -= 1;
    self.ready.push(OpCompleted {
      registration_id: entry.id,
      result,
      cancelled,
      _keep: keep,
    });
  }

  /// Reports the operation with `result`: non-negative for success, a
  /// negative errno for an error.
  ///
  /// # Safety
  ///
  /// - Nothing (this backend, the kernel, a worker thread) uses an address
  ///   copied from the operation's [`Op`] any more.
  /// - `result` is what happened. The driver reads it into the operation's
  ///   memory without checking: a non-negative result for an operation that
  ///   fills memory means that many bytes of it were written, and for one
  ///   that creates a resource it is a new descriptor or handle nothing else
  ///   owns, which [`IoBackend::resource_close`] closes.
  #[inline]
  pub unsafe fn complete(&mut self, key: OpKey, result: isize) {
    self.report(key, result, false);
  }

  /// Reports the operation as stopped by [`IoBackend::cancel`] before it
  /// finished. `result` is the error the platform has for that.
  ///
  /// # Safety
  ///
  /// Nothing uses an address copied from the operation's [`Op`] any more,
  /// and `result` is negative.
  #[inline]
  pub unsafe fn cancelled(&mut self, key: OpKey, result: isize) {
    self.report(key, result, true);
  }

  /// How many reports wait for the driver to collect them. A backend that
  /// would block in [`IoBackend::wait`] uses this to see that it should
  /// not: something is ready already.
  #[inline]
  pub fn waiting(&self) -> usize {
    self.ready.len()
  }

  /// The reports that wait for the driver to collect them, oldest first.
  #[inline]
  pub fn reported(&self) -> &[OpCompleted] {
    self.ready
  }
}

/// State a backend keeps for each operation it has started, indexed by
/// [`OpKey::slot`].
///
/// Looking up a key that is not the one stored in its slot finds nothing, so
/// a stale key never reaches another operation's state.
#[derive(Debug)]
pub struct PerOp<T> {
  slots: Vec<Option<(OpKey, T)>>,
  len: usize,
}

impl<T> Default for PerOp<T> {
  fn default() -> Self {
    Self { slots: Vec::new(), len: 0 }
  }
}

impl<T> PerOp<T> {
  /// Room for the operations of a backend initialized with capacity `cap`.
  pub fn new(cap: usize) -> Self {
    let mut slots = Vec::new();
    slots.resize_with(cap, || None);
    Self { slots, len: 0 }
  }

  /// Stores `value` for `key`, and returns what the slot held: the state of
  /// an operation the backend reported without taking its state out.
  #[inline]
  pub fn insert(&mut self, key: OpKey, value: T) -> Option<T> {
    if key.slot() >= self.slots.len() {
      self.grow(key.slot());
    }
    let slot = &mut self.slots[key.slot()];
    let old = slot.replace((key, value));
    if old.is_none() {
      self.len += 1;
    }
    old.map(|(_, value)| value)
  }

  /// Makes room for `slot`: one past the capacity this was made with.
  #[cold]
  #[inline(never)]
  fn grow(&mut self, slot: usize) {
    self.slots.resize_with(slot + 1, || None);
  }

  #[inline]
  pub fn get(&self, key: OpKey) -> Option<&T> {
    match self.slots.get(key.slot()) {
      Some(Some((stored, value))) if *stored == key => Some(value),
      _ => None,
    }
  }

  #[inline]
  pub fn get_mut(&mut self, key: OpKey) -> Option<&mut T> {
    match self.slots.get_mut(key.slot()) {
      Some(Some((stored, value))) if *stored == key => Some(value),
      _ => None,
    }
  }

  #[inline]
  pub fn contains(&self, key: OpKey) -> bool {
    self.get(key).is_some()
  }

  /// Takes the state stored for `key` out.
  #[inline]
  pub fn take(&mut self, key: OpKey) -> Option<T> {
    let slot = self.slots.get_mut(key.slot())?;
    if !matches!(slot, Some((stored, _)) if *stored == key) {
      return None;
    }
    self.len -= 1;
    slot.take().map(|(_, value)| value)
  }

  /// How many operations have state stored.
  #[inline]
  pub fn len(&self) -> usize {
    self.len
  }

  #[inline]
  pub fn is_empty(&self) -> bool {
    self.len == 0
  }

  /// The keys that have state stored, in slot order.
  pub fn keys(&self) -> impl Iterator<Item = OpKey> + '_ {
    self.slots.iter().flatten().map(|(key, _)| *key)
  }
}

/// Unified I/O backend trait: executes the operations the driver submits.
///
/// Designed for single-thread ownership (`&mut self`), dyn-compatible for
/// runtime backend selection via `Box<dyn IoBackend>`.
///
/// A backend is driven through a [`Tracked`], which owns the table of its
/// operations. [`Lio`](crate::Lio) holds one.
///
/// Contract:
/// - `init()` is called before anything else
/// - `start()` is called once per operation, in the order they were pushed,
///   when the driver flushes. It may perform the operation at once
/// - `wait()` hands whatever was started to the kernel, and reports what
///   has finished
///
/// # What an implementor promises
///
/// The trait is safe to implement. The driver frees an operation's buffers
/// once the operation is reported, and the two things it cannot check are
/// claimed where they are made, by calling an `unsafe` method of [`Done`]:
///
/// - reporting an operation says nothing (the backend, the kernel, a worker
///   thread) uses an address copied from its [`Op`] any more;
/// - [`Done::complete`] says the result is what happened.
///
/// An address copied from an [`Op`] may be used only between
/// [`Queued::begin`] and the report of the key that returned. That is the
/// rule the `unsafe` blocks of a backend answer to.
///
/// A backend never holds the right to report an operation, only its key.
/// The table ignores a key it does not hold as started, so a second report
/// does nothing. An operation that is never reported keeps its memory:
/// forgetting one is a leak, never a use after free.
///
/// # Shutdown
///
/// There is no method for it. When the driver is dropped it
/// [`cancel`](IoBackend::cancel)s everything it has not seen a completion
/// for and [`wait`](IoBackend::wait)s, for a bounded time, for those
/// completions. It then drops the backend if every operation was reported,
/// and leaks it, with the operations' memory, if one was not. A backend
/// whose `cancel` and `wait` keep their contracts shuts down correctly.
pub trait IoBackend {
  /// Initialize backend with capacity for `cap` concurrent operations.
  ///
  /// Pre-allocates all resources for zero-allocation runtime.
  /// Must be called once before any other methods.
  fn init(&mut self, cap: usize) -> io::Result<()>;

  /// How resources produced by this backend's completions are closed.
  ///
  /// [`ResourceClose::native`] closes them as OS descriptors or handles. A
  /// backend whose completions do not carry real OS resources (such as a
  /// simulation) must not return that, or dropping those resources would
  /// close unrelated descriptors of the process.
  fn resource_close(&self) -> ResourceClose;

  /// Takes one operation: [`begin`](Queued::begin) it, or
  /// [`refuse`](Queued::refuse) it.
  ///
  /// A begun operation may be performed here and reported through `done`
  /// right away, or handed to the kernel or a worker and reported from a
  /// later call. `step_bump` is the operation's own arena: what is
  /// allocated there stays in place until the operation is reported.
  ///
  /// Never more than the initialized capacity are begun and unreported at
  /// once.
  fn start(
    &mut self,
    op: Queued<'_>,
    step_bump: &mut Bump,
    done: &mut Done<'_>,
  ) -> Started;

  /// Asks the backend to finish a started operation early: its consumer is
  /// gone, or the driver is shutting down.
  ///
  /// The operation must still be reported exactly once, from this call or a
  /// later one: with [`Done::cancelled`], or with its own result if it
  /// finished first. Until then the memory its [`Op`] points to stays valid.
  ///
  /// `key` is always one the backend began and the table has not seen
  /// reported. The driver may ask again for an operation it already asked
  /// about.
  ///
  /// A backend may do nothing here only if every operation it accepts
  /// finishes on its own. One that may never complete (an accept, a read
  /// from an idle socket) would otherwise keep its slot forever, and its
  /// memory after the driver is gone.
  fn cancel(&mut self, key: OpKey, done: &mut Done<'_>);

  /// Wait for completions with optional timeout, and report them through
  /// `done`.
  ///
  /// - `None` = block until at least one operation is reported
  /// - `Some(ZERO)` = non-blocking poll
  /// - `Some(duration)` = wait up to duration
  ///
  /// [`Done::waiting`] counts reports the driver has not collected yet,
  /// from `start`, `cancel` or this call. A backend must not block while
  /// there are any.
  fn wait(
    &mut self,
    timeout: Option<Duration>,
    done: &mut Done<'_>,
  ) -> io::Result<()>;
}

impl<B: IoBackend + ?Sized> IoBackend for Box<B> {
  #[inline]
  fn init(&mut self, cap: usize) -> io::Result<()> {
    (**self).init(cap)
  }

  #[inline]
  fn resource_close(&self) -> ResourceClose {
    (**self).resource_close()
  }

  #[inline]
  fn start(
    &mut self,
    op: Queued<'_>,
    step_bump: &mut Bump,
    done: &mut Done<'_>,
  ) -> Started {
    (**self).start(op, step_bump, done)
  }

  #[inline]
  fn cancel(&mut self, key: OpKey, done: &mut Done<'_>) {
    (**self).cancel(key, done)
  }

  #[inline]
  fn wait(
    &mut self,
    timeout: Option<Duration>,
    done: &mut Done<'_>,
  ) -> io::Result<()> {
    (**self).wait(timeout, done)
  }
}

/// A backend, and the table of the operations it has been given.
///
/// This is how a backend is driven: by [`Lio`](crate::Lio), and by anything
/// that uses one without it (contract tests, benchmarks).
///
/// An operation is named by the id it was pushed with. The low 32 bits of
/// that id are its slot, which must not be in use by another operation that
/// has not been reported, and should be small: the table is indexed by it.
/// The ids `Lio` uses are made that way, and small numbers are too.
///
/// - [`push`](Self::push) queues an operation. The backend does not see it.
/// - [`flush`](Self::flush) gives the backend what is queued, in order.
/// - [`cancel`](Self::cancel) reports a queued operation itself, and asks
///   the backend about a started one.
/// - [`wait`](Self::wait) collects what was reported.
pub struct Tracked<B> {
  // Before the table: a backend that stops its work when dropped may still
  // touch what it allocated in the arenas.
  backend: B,
  entries: Vec<Entry>,
  /// One per slot: the operation there, while it is queued. Apart from the
  /// entries, so that a backend can read one while it reports another.
  ops: Vec<Op>,
  /// One per slot: the step arena of the operation there.
  arenas: Vec<Bump>,
  /// Queued operations, in push order. Those before `next` were handed out.
  backlog: Vec<OpKey>,
  next: usize,
  cap: usize,
  /// Operations queued or started, and not reported.
  live: usize,
  /// Reported, and not collected by `wait` yet.
  ready: Vec<OpCompleted>,
}

impl<B: IoBackend> Tracked<B> {
  /// Wraps `backend`. [`init`](Self::init) comes next.
  pub fn new(backend: B) -> Self {
    Self {
      backend,
      entries: Vec::new(),
      ops: Vec::new(),
      arenas: Vec::new(),
      backlog: Vec::new(),
      next: 0,
      cap: 0,
      live: 0,
      ready: Vec::new(),
    }
  }

  /// Initializes the backend and the table for `cap` operations at once.
  ///
  /// # Panics
  ///
  /// If an operation is in flight: its lease is in the table, and the
  /// backend may be using its memory.
  pub fn init(&mut self, cap: usize) -> io::Result<()> {
    assert!(
      self.live == 0,
      "IoBackend: initialized with {} operations in flight",
      self.live
    );
    self.backend.init(cap)?;
    self.entries.clear();
    self.entries.resize_with(cap, Entry::vacant);
    self.ops.clear();
    self.ops.resize_with(cap, || Op::Nop);
    self.arenas.clear();
    self.arenas.resize_with(cap, Bump::new);
    self.backlog = Vec::with_capacity(cap);
    self.next = 0;
    self.cap = cap;
    self.live = 0;
    self.ready = Vec::with_capacity(cap);
    Ok(())
  }

  /// Takes the table apart: the backend, and the step arenas.
  ///
  /// The driver puts operations' memory in the arenas, so it drops them
  /// after its registrations, and the backend before both.
  pub(crate) fn into_parts(self) -> (B, Vec<Bump>) {
    (self.backend, self.arenas)
  }

  /// The backend.
  #[inline]
  pub fn backend(&self) -> &B {
    &self.backend
  }

  /// The backend, to reach what it offers beyond [`IoBackend`]. Its
  /// operations are driven through this table only.
  #[inline]
  pub fn backend_mut(&mut self) -> &mut B {
    &mut self.backend
  }

  /// How many operations are queued or started, and not reported.
  #[inline]
  pub fn in_flight(&self) -> usize {
    self.live
  }

  /// The slot of `id`, with room made for it in the table.
  #[inline]
  fn slot_of(&mut self, id: u64) -> usize {
    let slot = id as u32 as usize;
    if slot >= self.entries.len() {
      self.grow(slot);
    }
    slot
  }

  /// Makes room for `slot`: one past the capacity, which the driver uses in
  /// place of a slot whose ids ran out.
  #[cold]
  #[inline(never)]
  fn grow(&mut self, slot: usize) {
    self.entries.resize_with(slot + 1, Entry::vacant);
    self.ops.resize_with(slot + 1, || Op::Nop);
    self.arenas.resize_with(slot + 1, Bump::new);
  }

  /// The step arena of the slot of `id`.
  ///
  /// The driver puts an operation's memory there before it pushes it, and
  /// resets it before the next operation in that slot.
  #[inline]
  pub(crate) fn arena(&mut self, id: u64) -> &mut Bump {
    let slot = self.slot_of(id);
    &mut self.arenas[slot]
  }

  /// Queues an operation borrowed from a model the driver has pinned.
  ///
  /// The driver calls this while it borrows the operation's memory to build
  /// the action, and does not move or drop that memory while `keep` is held
  /// elsewhere. That is what lets the borrow end here.
  #[inline]
  pub(crate) fn push_model(&mut self, id: u64, op: BorrowedOp<'_>, keep: Keep) {
    assert!(
      self.live < self.cap,
      "IoBackend capacity exceeded: attempted to queue more than {} operations",
      self.cap
    );
    let slot = self.slot_of(id);
    let entry = &mut self.entries[slot];
    assert!(
      entry.state == State::Vacant,
      "IoBackend: pushed operation {id} while operation {} is still in flight \
       in its slot",
      entry.id
    );
    self.live += 1;
    entry.generation = entry.generation.wrapping_add(1);
    entry.state = State::Queued;
    entry.id = id;
    entry.keep = Some(keep);
    self.backlog.push(OpKey::new(slot as u32, entry.generation));
    self.ops[slot].clone_from(op.as_op());
  }

  /// Queues an operation whose buffers the caller keeps alive itself.
  ///
  /// Contract tests use this. [`Lio`](crate::Lio) does not: it submits the
  /// model it owns.
  ///
  /// # Safety
  ///
  /// The addresses in `op` stay allocated and unmoved, and nothing but the
  /// backend mutates them, until the [`OpCompleted`] for `id` was dropped.
  /// Dropping this `Tracked` first ends that only as far as the backend's
  /// own `Drop` stops its work, which the backends in
  /// [`impls`](crate::backend::impls) do.
  ///
  /// # Panics
  ///
  /// If as many operations as the capacity are in flight already, or the
  /// slot of `id` holds one of them.
  pub unsafe fn push(&mut self, id: u64, op: BorrowedOp<'_>) {
    // Checked first: the arena of a slot that is in use holds what the
    // backend lowered for the operation there.
    self.push_model(id, op, Keep::new());
    // The backend does not see this operation before the flush.
    self.arena(id).reset();
  }

  /// Gives the backend every queued operation, in the order they were
  /// pushed.
  pub fn flush(&mut self) {
    let Self {
      backend, entries, ops, arenas, backlog, next, ready, live, ..
    } = self;
    // The cursor is kept in `self`: a backend that panics leaves the rest
    // queued for the next flush.
    while let Some(&key) = backlog.get(*next) {
      *next += 1;
      let slot = key.slot();
      let entry = &mut entries[slot];
      if entry.generation != key.generation() || entry.state != State::Queued {
        continue; // Cancelled while it was queued.
      }
      // From here on it is the backend's, unless the backend refuses it.
      entry.state = State::Started;
      let queued = Queued { key, id: entry.id, op: &mut ops[slot] };
      let mut done = Done {
        entries: &mut entries[..],
        ready: &mut *ready,
        live: &mut *live,
      };
      let started = backend.start(queued, &mut arenas[slot], &mut done);
      if let Outcome::Refused(result) = started.0 {
        // The backend never had the key, so it cannot have reported it.
        done.report(key, result, false);
      }
      // What the backend did not take goes, with the resources it holds.
      ops[slot] = Op::Nop;
    }
    backlog.clear();
    *next = 0;
  }

  /// Asks for operation `id` to finish early.
  ///
  /// One that is still queued is reported as cancelled, with
  /// [`CANCELLED_BEFORE_START`], and never reaches the backend. One the
  /// backend has is its to stop: see [`IoBackend::cancel`]. Anything else
  /// (unknown, or reported already) is left alone.
  pub fn cancel(&mut self, id: u64) {
    let slot = id as u32 as usize;
    let Some(entry) = self.entries.get_mut(slot) else {
      return;
    };
    if entry.id != id {
      return;
    }
    let key = OpKey::new(slot as u32, entry.generation);
    match entry.state {
      State::Vacant => {}
      State::Queued => {
        // No address ever left the table.
        entry.state = State::Started;
        self.ops[slot] = Op::Nop;
        let mut done = Done {
          entries: &mut self.entries,
          ready: &mut self.ready,
          live: &mut self.live,
        };
        done.report(key, CANCELLED_BEFORE_START, true);
      }
      State::Started => {
        let mut done = Done {
          entries: &mut self.entries,
          ready: &mut self.ready,
          live: &mut self.live,
        };
        self.backend.cancel(key, &mut done);
      }
    }
  }

  /// Waits for the backend, then moves everything reported so far into
  /// `completed`, which is cleared first.
  ///
  /// What was reported is moved even when the backend fails, so those
  /// operations are not lost.
  pub fn wait(
    &mut self,
    timeout: Option<Duration>,
    completed: &mut Vec<OpCompleted>,
  ) -> io::Result<()> {
    completed.clear();
    // What was reported since the last wait is in `ready`: the backend
    // reports the rest straight into `completed`.
    std::mem::swap(completed, &mut self.ready);
    let mut done = Done {
      entries: &mut self.entries,
      ready: completed,
      live: &mut self.live,
    };
    self.backend.wait(timeout, &mut done)
  }

  /// Queues a completion for an operation the table does not hold, to test
  /// what the driver does with one.
  #[cfg(test)]
  pub(crate) fn forge(&mut self, id: u64, result: isize) {
    self.ready.push(OpCompleted {
      registration_id: id,
      result,
      cancelled: false,
      _keep: Keep::new(),
    });
  }
}

impl<B> std::fmt::Debug for Tracked<B> {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("Tracked")
      .field("capacity", &self.entries.len())
      .field("queued", &(self.backlog.len() - self.next))
      .field("ready", &self.ready.len())
      .finish_non_exhaustive()
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Begins every operation, and panics on the one named.
  #[derive(Default)]
  struct Fussy {
    panic_on: Option<u64>,
    begun: Vec<(u64, OpKey)>,
  }

  impl IoBackend for Fussy {
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
      let id = op.id();
      assert!(self.panic_on != Some(id), "fussy about {id}");
      let (started, key, _) = op.begin();
      self.begun.push((id, key));
      started
    }
    fn cancel(&mut self, key: OpKey, done: &mut Done<'_>) {
      // SAFETY: a nop has no addresses, and the result is negative.
      unsafe { done.cancelled(key, -1) };
    }
    fn wait(
      &mut self,
      _: Option<Duration>,
      _: &mut Done<'_>,
    ) -> io::Result<()> {
      Ok(())
    }
  }

  fn tracked(cap: usize) -> Tracked<Fussy> {
    let mut io = Tracked::new(Fussy::default());
    assert!(io.init(cap).is_ok());
    io
  }

  fn push(io: &mut Tracked<Fussy>, id: u64) {
    // SAFETY: a nop has no addresses.
    unsafe { io.push(id, BorrowedOp::nop()) };
  }

  fn collect(io: &mut Tracked<Fussy>) -> Vec<(u64, isize, bool)> {
    let mut completed = Vec::new();
    assert!(io.wait(Some(Duration::ZERO), &mut completed).is_ok());
    completed
      .iter()
      .map(|done| (done.registration_id(), done.result(), done.is_cancelled()))
      .collect()
  }

  #[test]
  fn a_key_packs_its_slot_and_survives_the_kernel() {
    let key = OpKey::new(7, 3);
    assert_eq!((key.slot(), key.generation()), (7, 3));
    assert_eq!(OpKey::from_u64(key.as_u64()), key);
    assert_ne!(OpKey::new(7, 4), key, "the slot's next operation");
  }

  #[test]
  fn operations_are_started_in_push_order_and_counted() {
    let mut io = tracked(4);
    for id in [2, 0, 3] {
      push(&mut io, id);
    }
    assert_eq!(io.in_flight(), 3);
    assert!(io.backend().begun.is_empty(), "nothing before the flush");
    io.flush();
    let ids: Vec<u64> = io.backend().begun.iter().map(|(id, _)| *id).collect();
    assert_eq!(ids, [2, 0, 3]);
    io.flush();
    assert_eq!(io.backend().begun.len(), 3, "each is started once");
  }

  #[test]
  fn a_cancel_goes_to_whoever_has_the_operation() {
    let mut io = tracked(4);
    push(&mut io, 1);
    io.flush();
    push(&mut io, 2);
    io.cancel(1); // The backend has it.
    io.cancel(2); // Still queued.
    io.cancel(3); // Unknown.
    assert_eq!(
      collect(&mut io),
      [(1, -1, true), (2, CANCELLED_BEFORE_START, true)]
    );
    assert_eq!(io.in_flight(), 0);
    io.flush();
    assert_eq!(io.backend().begun.len(), 1, "the cancelled one never started");
    io.cancel(1);
    assert!(collect(&mut io).is_empty(), "a reported operation is left alone");
  }

  #[test]
  #[cfg_attr(not(panic = "unwind"), ignore = "catches a panic")]
  fn pushing_an_id_in_flight_leaves_its_arena_alone() {
    let mut io = tracked(2);
    push(&mut io, 1);
    io.flush();
    let at = |io: &mut Tracked<Fussy>| io.arena(1).alloc(7u64) as *mut u64;
    let first = at(&mut io);
    let again = std::panic::AssertUnwindSafe(|| push(&mut io, 1));
    assert!(std::panic::catch_unwind(again).is_err());
    assert_ne!(at(&mut io), first, "what was allocated there is still there");
    assert_eq!(io.in_flight(), 1);

    // A vacant slot's arena is reset for the operation pushed into it.
    io.cancel(1);
    assert_eq!(collect(&mut io).len(), 1);
    push(&mut io, 1);
    assert_eq!(at(&mut io), first);
  }

  #[test]
  fn a_cancel_names_the_operation_by_its_whole_id() {
    let mut io = tracked(4);
    push(&mut io, 1);
    io.flush();
    // Same slot, another generation of the driver's id.
    io.cancel(1 | 1 << 32);
    assert!(collect(&mut io).is_empty());
    assert_eq!(io.in_flight(), 1);
  }

  #[test]
  #[cfg_attr(not(panic = "unwind"), ignore = "catches a panic")]
  fn a_backend_that_panics_leaves_the_rest_queued() {
    let mut io = tracked(4);
    io.backend_mut().panic_on = Some(2);
    for id in [1, 2, 3] {
      push(&mut io, id);
    }
    let flush = std::panic::AssertUnwindSafe(|| io.flush());
    assert!(std::panic::catch_unwind(flush).is_err());
    assert_eq!(io.backend().begun.len(), 1);
    io.flush();
    let ids: Vec<u64> = io.backend().begun.iter().map(|(id, _)| *id).collect();
    assert_eq!(ids, [1, 3], "the one it panicked on is not offered again");
    assert_eq!(io.in_flight(), 3, "and stays unreported: its memory is kept");
  }

  #[test]
  fn the_table_makes_room_for_a_slot_past_its_capacity() {
    let mut io = tracked(1);
    push(&mut io, 5);
    io.flush();
    let (_, key) = io.backend().begun[0];
    assert_eq!(key.slot(), 5);
    io.cancel(5);
    assert_eq!(collect(&mut io), [(5, -1, true)]);
  }

  #[test]
  fn per_op_state_is_found_only_by_the_key_it_was_stored_for() {
    let mut state = PerOp::new(2);
    let (first, next) = (OpKey::new(1, 1), OpKey::new(1, 2));
    assert!(state.is_empty());
    assert_eq!(state.insert(first, 'a'), None);
    assert_eq!((state.len(), state.contains(first)), (1, true));
    assert_eq!(state.get(next), None, "a stale key finds nothing");
    assert_eq!(state.get_mut(next), None);
    assert_eq!(state.take(next), None);
    assert_eq!(state.get(first), Some(&'a'));
    if let Some(value) = state.get_mut(first) {
      *value = 'b';
    }

    // The slot's next operation replaces what was left there.
    assert_eq!(state.insert(next, 'c'), Some('b'));
    assert_eq!(state.len(), 1);
    assert_eq!(state.take(first), None);
    assert_eq!(state.take(next), Some('c'));
    assert!(state.is_empty());
    assert_eq!(state.take(OpKey::new(9, 1)), None, "past the capacity");
  }

  #[test]
  fn per_op_state_grows_and_lists_its_keys_in_slot_order() {
    let mut state = PerOp::default();
    let keys = [OpKey::new(4, 1), OpKey::new(0, 1), OpKey::new(2, 7)];
    for (value, key) in keys.into_iter().enumerate() {
      state.insert(key, value);
    }
    assert_eq!(state.len(), 3);
    assert_eq!(state.keys().collect::<Vec<_>>(), [keys[1], keys[2], keys[0]]);
    assert_eq!(state.take(keys[0]), Some(0));
    assert_eq!(state.keys().collect::<Vec<_>>(), [keys[1], keys[2]]);
  }
}
