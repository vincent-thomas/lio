//! Guest memory as a lio buffer.
//!
//! A lio read owns its buffer until the kernel is done with it. A guest's
//! read names a range of its linear memory. [`GuestBuf`] is that range,
//! given to lio, so the kernel writes where the guest will look and nothing
//! is copied.
//!
//! What makes that sound is kept by three types:
//!
//! - [`Engine`] is the only way to get one, and it turns memory relocation
//!   off: a linear memory stays at its address for as long as it exists.
//! - [`Store`] is the only kind of store whose guests are lent from. It does
//!   not free its memories, or give their pool slots to another instance,
//!   until every [`GuestBuf`] has come back from lio. Leaking one keeps the
//!   memory for good.
//! - A [`Lease`] stands for one read. The stream that started it does not
//!   tell the guest the read is over until the lease's buffer has come
//!   back, so the guest gets its range back only once the kernel has let go.

use std::cell::{Cell, RefCell};
use std::io;
use std::mem::{ManuallyDrop, MaybeUninit};
use std::ops::{Deref, DerefMut};
use std::rc::{Rc, Weak};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use lio::api::Io;
use lio::model::{OpModel, Task};
use lio::{Initialized, IoBuf, IoBufMut, Lio};

use crate::WasiCtx;
use crate::local::Local;

/// An engine whose linear memories never move.
#[derive(Clone)]
pub struct Engine(wasmtime::Engine);

impl Engine {
  /// Builds an engine from `config`, with the component model's async
  /// support on and memory relocation off.
  ///
  /// With relocation off, a memory that outgrows what was reserved for it
  /// fails to grow instead of moving. wasmtime's default reservation on
  /// 64-bit hosts covers all of a 32-bit guest's address space.
  ///
  /// # Errors
  ///
  /// When the configuration is not valid for this host.
  pub fn new(mut config: wasmtime::Config) -> wasmtime::Result<Self> {
    config.wasm_component_model_async(true);
    config.memory_may_move(false);
    Ok(Self(wasmtime::Engine::new(&config)?))
  }
}

impl Deref for Engine {
  type Target = wasmtime::Engine;

  fn deref(&self) -> &wasmtime::Engine {
    &self.0
  }
}

/// A store whose guests' memory lio operations write into directly.
///
/// It is a [`wasmtime::Store`] in every other way.
pub struct Store<T: 'static> {
  /// Dropped by hand, after the last buffer has come back.
  store: ManuallyDrop<wasmtime::Store<T>>,
  lending: LendingRef,
  lio: Lio,
}

impl<T: 'static> Store<T> {
  /// Makes a store for `engine`. `ctx` finds the [`WasiCtx`] in `data`,
  /// and `lio` is the driver the host's operations run on.
  pub fn new(
    engine: &Engine,
    mut data: T,
    lio: &Lio,
    ctx: fn(&mut T) -> &mut WasiCtx,
  ) -> Self {
    let lending = Local::new(Rc::new(Lending {
      lio: lio.clone(),
      lent: Cell::new(0),
      live: RefCell::new(Vec::new()),
      spare: RefCell::new(Vec::new()),
      closed: Cell::new(false),
    }));
    ctx(&mut data).lending = Some(lending.clone());
    Self {
      store: ManuallyDrop::new(wasmtime::Store::new(engine, data)),
      lending,
      lio: lio.clone(),
    }
  }
}

impl<T: 'static> Deref for Store<T> {
  type Target = wasmtime::Store<T>;

  fn deref(&self) -> &wasmtime::Store<T> {
    &self.store
  }
}

impl<T: 'static> DerefMut for Store<T> {
  fn deref_mut(&mut self) -> &mut wasmtime::Store<T> {
    &mut self.store
  }
}

impl<T: 'static> Drop for Store<T> {
  fn drop(&mut self) {
    // Ask every read in flight to stop, then run the driver until the
    // kernel has let go of each buffer. Only then are the memories freed.
    self.lending.cancel_all();
    while self.lending.lent.get() > 0 {
      if self.lio.run_timeout(Duration::from_millis(10)).is_err() {
        // The driver is broken, so nothing says the kernel is done. The
        // memories are kept: a leak is the safe outcome.
        return;
      }
    }
    self.lending.close();
    // SAFETY: dropped once, here, and not used again.
    unsafe { ManuallyDrop::drop(&mut self.store) };
  }
}

thread_local! {
  static STATS: Cell<Stats> = const {
    Cell::new(Stats {
      reads_lent: 0,
      reads_copied: 0,
      writes_lent: 0,
      writes_copied: 0,
    })
  };
}

/// How many stream reads and writes this thread's host has done each way.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
  /// Reads the kernel made straight into guest memory.
  pub reads_lent: usize,
  /// Reads through a host buffer, copied to the reader.
  pub reads_copied: usize,
  /// Writes the kernel made straight from guest memory.
  pub writes_lent: usize,
  /// Writes copied to a host buffer first.
  pub writes_copied: usize,
}

/// The counts so far, on this thread.
pub fn stats() -> Stats {
  STATS.with(Cell::get)
}

/// Counts one read or write.
#[inline]
pub(crate) fn count(which: impl FnOnce(&mut Stats)) {
  STATS.with(|stats| {
    let mut now = stats.get();
    which(&mut now);
    stats.set(now);
  });
}

/// The guest buffers one store has out with lio.
///
/// It belongs to the host's thread, like everything that reaches it.
pub(crate) struct Lending {
  /// The driver the store's operations run on.
  lio: Lio,
  /// Buffers lio holds.
  lent: Cell<usize>,
  /// The streams whose read or write had to wait at least once.
  live: RefCell<Vec<Weak<Lease>>>,
  /// Leases of streams that are gone, for the next streams.
  spare: RefCell<Vec<Rc<Lease>>>,
  /// The store is going: nothing is kept for a next stream.
  closed: Cell<bool>,
}

/// How many leases wait for a next stream.
const SPARE: usize = 64;

/// A store's [`Lending`], where a `Send` value has to hold it.
pub(crate) type LendingRef = Local<Rc<Lending>>;

impl Lending {
  /// Starts a read into `room`, a range of guest memory.
  ///
  /// `start` makes the operation from the buffer. Its result arrives in
  /// `stream`'s lease: a stream has one operation at a time, so it keeps
  /// one lease and uses it again. Returns whether the operation is over
  /// already, which most are.
  ///
  /// # Safety
  ///
  /// `room` is the buffer of a guest read on a [`Store`] made from an
  /// [`Engine`], and that read is not reported to the guest as over until
  /// the lease's [`poll`](Lease::poll) has returned `Ready`.
  pub(crate) unsafe fn read<M>(
    self: &Rc<Self>,
    stream: &mut StreamLease,
    room: &mut [u8],
    start: impl FnOnce(GuestBuf) -> Io<M>,
  ) -> bool
  where
    M: OpModel<Item = (io::Result<usize>, GuestBuf)> + 'static,
  {
    count(|stats| stats.reads_lent += 1);
    self.lend(stream, room.as_mut_ptr(), room.len(), 0, start)
  }

  /// Starts a write of `data`, a range of guest memory, like
  /// [`read`](Self::read).
  ///
  /// # Safety
  ///
  /// As for [`read`](Self::read), for a guest write: `data` is the guest's
  /// own memory, not a host buffer.
  pub(crate) unsafe fn write<M>(
    self: &Rc<Self>,
    stream: &mut StreamLease,
    data: &[u8],
    start: impl FnOnce(GuestBuf) -> Io<M>,
  ) -> bool
  where
    M: OpModel<Item = (io::Result<usize>, GuestBuf)> + 'static,
  {
    count(|stats| stats.writes_lent += 1);
    // The buffer is only ever read through this pointer: a write uses
    // `IoBuf::as_slice`, never `IoBufMut`.
    self.lend(stream, data.as_ptr().cast_mut(), data.len(), data.len(), start)
  }

  fn lend<M>(
    self: &Rc<Self>,
    stream: &mut StreamLease,
    ptr: *mut u8,
    len: usize,
    filled: usize,
    start: impl FnOnce(GuestBuf) -> Io<M>,
  ) -> bool
  where
    M: OpModel<Item = (io::Result<usize>, GuestBuf)> + 'static,
  {
    let lease = stream.0.get_or_insert_with(|| {
      // One a stream that is gone left behind, or a new one.
      let spare = self.spare.borrow_mut().pop();
      LeaseRef(Local::new(spare.unwrap_or_else(|| {
        Rc::new(Lease {
          flags: Cell::new(RELEASED),
          count: Cell::new(0),
          tracked: Cell::new(false),
          error: Cell::new(None),
          task: Cell::new(None),
          waker: Cell::new(None),
          lending: Rc::clone(self),
        })
      })))
    });
    // The stream's last operation is over: its buffer came back before the
    // stream was told so.
    assert!(
      lease.flags.get() & RELEASED != 0,
      "a stream started an operation with one still in flight"
    );
    lease.flags.set(0);
    self.lent.set(self.lent.get() + 1);
    let buf = GuestBuf { ptr: Ptr(ptr), len, filled, lease: lease.clone() };
    // The buffer carries the lease, so the callback needs nothing else.
    let task = start(buf).with_lio(&self.lio).when_done(
      |(result, buf): (io::Result<usize>, GuestBuf)| {
        buf.lease.finish(result);
        // Gives the range back, and wakes the stream.
        drop(buf);
      },
    );
    // Only one still in flight can be stopped. Most are done already.
    let done = lease.flags.get() & RELEASED != 0;
    if !done {
      lease.task.set(Some(task));
    }
    done
  }

  fn cancel_all(&self) {
    let live = std::mem::take(&mut *self.live.borrow_mut());
    for lease in live.iter().filter_map(Weak::upgrade) {
      lease.cancel();
    }
  }

  /// Nothing more is kept for a next stream, and what was kept goes: a
  /// lease holds its store's lending, which would otherwise never be freed.
  fn close(&self) {
    self.closed.set(true);
    let spare = std::mem::take(&mut *self.spare.borrow_mut());
    drop(spare);
  }
}

/// The kernel is done with the range: lio has dropped the buffer.
const RELEASED: u8 = 1;
/// The operation was asked to stop.
const CANCELLED: u8 = 1 << 1;
/// The operation returned. Its count is in `count`, or its error in `error`.
const DONE: u8 = 1 << 2;
const FAILED: u8 = 1 << 3;

/// A stream's read or write into guest memory, one at a time.
///
/// It belongs to the host's thread: a [`LeaseRef`] is how a `Send` value
/// holds one.
pub(crate) struct Lease {
  flags: Cell<u8>,
  /// The bytes an operation that returned moved, or the bytes the kernel
  /// had written when the buffer of a cancelled read came back.
  count: Cell<usize>,
  /// The store has this lease in its list of ones to stop.
  tracked: Cell<bool>,
  error: Cell<Option<io::Error>>,
  /// Stops the operation, while it is in flight.
  task: Cell<Option<Task>>,
  /// Wakes the stream when the buffer comes back.
  waker: Cell<Option<Waker>>,
  lending: Rc<Lending>,
}

/// A [`Lease`], where a `Send` value has to hold it.
#[derive(Clone)]
struct LeaseRef(Local<Rc<Lease>>);

impl Deref for LeaseRef {
  type Target = Lease;

  #[inline]
  fn deref(&self) -> &Lease {
    &self.0
  }
}

/// A stream's lease, once its first read or write into guest memory has
/// made one. When the stream goes, the lease is left for a next stream.
#[derive(Default)]
pub(crate) struct StreamLease(Option<LeaseRef>);

impl StreamLease {
  /// The lease, if the stream has one.
  #[inline]
  pub(crate) fn get(&self) -> Option<&Lease> {
    self.0.as_deref()
  }

  /// Remembers the lease with its store, so that a store being dropped
  /// can stop what is in flight on it. An operation that finished as it
  /// started needs no remembering, which is most of them.
  pub(crate) fn track(&self) {
    let Some(lease) = &self.0 else { return };
    if lease.tracked.replace(true) {
      return;
    }
    let mut live = lease.lending.live.borrow_mut();
    live.retain(|lease| lease.strong_count() > 0);
    live.push(Rc::downgrade(&lease.0));
  }
}

impl Drop for StreamLease {
  fn drop(&mut self) {
    let Some(LeaseRef(lease)) = self.0.take() else { return };
    let lending = &lease.lending;
    // Not in flight, and nothing else holds it: the next stream can have
    // it.
    let idle = Rc::strong_count(&lease) == 1
      && lease.flags.get() & RELEASED != 0
      && !lending.closed.get();
    if idle && lending.spare.borrow().len() < SPARE {
      lease.error.set(None);
      lease.waker.set(None);
      let lending = Rc::clone(lending);
      lending.spare.borrow_mut().push(Rc::clone(&lease));
    }
  }
}

/// How a read into guest memory ended.
pub(crate) enum Lent {
  /// The operation finished, with this result.
  Done(io::Result<usize>),
  /// It was stopped before the kernel wrote anything.
  Cancelled,
}

impl Lease {
  /// Records what the operation returned.
  fn finish(&self, result: io::Result<usize>) {
    match result {
      Ok(count) => {
        self.count.set(count);
        self.flags.set(self.flags.get() | DONE);
      }
      Err(error) => {
        self.error.set(Some(error));
        self.flags.set(self.flags.get() | DONE | FAILED);
      }
    }
  }

  /// Whether the kernel is done with the range, and how the operation
  /// ended.
  ///
  /// With `finish`, the operation is asked to stop first. It is still
  /// `Pending` until the kernel has let go. A read the kernel finished
  /// before the stop reached it reports the bytes it wrote.
  pub(crate) fn poll(&self, cx: &mut Context<'_>, finish: bool) -> Poll<Lent> {
    if finish {
      self.cancel();
    }
    let flags = self.flags.get();
    if flags & RELEASED == 0 {
      self.waker.set(Some(cx.waker().clone()));
      return Poll::Pending;
    }
    let count = self.count.get();
    Poll::Ready(if flags & FAILED != 0 {
      let error = self.error.take();
      Lent::Done(Err(error.unwrap_or_else(|| io::Error::other("lost error"))))
    } else if flags & DONE != 0 || count > 0 {
      Lent::Done(Ok(count))
    } else if flags & CANCELLED != 0 {
      Lent::Cancelled
    } else {
      Lent::Done(Err(io::Error::other(
        "the driver dropped a read it had not finished",
      )))
    })
  }

  /// Whether the operation has already ended.
  pub(crate) fn is_done(&self) -> bool {
    self.flags.get() & RELEASED != 0
  }

  /// Asks the operation to stop. Its end still arrives through
  /// [`poll`](Self::poll), or, with nobody left to poll, is waited for by
  /// the store.
  pub(crate) fn cancel(&self) {
    if self.flags.get() & RELEASED != 0 {
      return;
    }
    self.flags.set(self.flags.get() | CANCELLED);
    // Stopping may drop the buffer at once, which reads these cells.
    if let Some(task) = self.task.take() {
      task.cancel();
    }
  }
}

/// A pointer into guest memory, sent to the kernel by the driver's thread.
struct Ptr(*mut u8);

// SAFETY: the pointer is only dereferenced by lio, on the driver's thread,
// and by the kernel. The range it names is not freed or moved while the
// buffer exists (see the module documentation).
unsafe impl Send for Ptr {}
// SAFETY: as above; nothing reads through a shared reference to it.
unsafe impl Sync for Ptr {}

/// A range of a guest's linear memory, lent to one lio read or write.
pub struct GuestBuf {
  ptr: Ptr,
  len: usize,
  filled: usize,
  lease: LeaseRef,
}

// SAFETY: the range stays mapped at this address until this buffer is
// dropped: its engine does not move memories, and its store frees them only
// once every buffer is back.
unsafe impl IoBuf for GuestBuf {
  fn as_slice(&self) -> &[u8] {
    // SAFETY: the first `filled` bytes were written by a read, or are the
    // bytes of a write, within the range.
    unsafe { std::slice::from_raw_parts(self.ptr.0, self.filled) }
  }
}

// SAFETY: `as_uninit_mut` is the whole range, and `commit` records only the
// length lio proves.
unsafe impl IoBufMut for GuestBuf {
  fn as_uninit_mut(&mut self) -> &mut [MaybeUninit<u8>] {
    // SAFETY: the range is `len` bytes of mapped guest memory, lent to this
    // read alone. Any byte pattern is a valid `MaybeUninit<u8>`.
    unsafe { std::slice::from_raw_parts_mut(self.ptr.0.cast(), self.len) }
  }

  fn commit(&mut self, initialized: Initialized<'_>) {
    assert!(
      initialized.as_ptr() == self.ptr.0 && initialized.len() <= self.len,
      "a proof for another buffer"
    );
    self.filled = initialized.len();
  }
}

impl Drop for GuestBuf {
  fn drop(&mut self) {
    let lease = &self.lease;
    // A read that was stopped still says how far the kernel got. One that
    // returned has said so itself.
    let flags = lease.flags.get();
    if flags & DONE == 0 {
      lease.count.set(self.filled);
    }
    lease.flags.set(flags | RELEASED);
    lease.task.set(None);
    let lending = &lease.lending;
    lending.lent.set(lending.lent.get() - 1);
    if let Some(waker) = lease.waker.take() {
      waker.wake();
    }
  }
}
