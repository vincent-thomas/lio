//! # lio-wasi
//!
//! A completion queue over WASI 0.3, built on the component model's async
//! built-ins instead of an executor.
//!
//! `wasip3` exposes WASI 0.3 as Rust futures and streams that read into an
//! owned `Vec`. Underneath, the component model is already shaped like a
//! completion queue: `stream.read` takes a pointer and a length, and a
//! `waitable-set` reports which blocked operation finished. This crate
//! exposes that layer directly, the way `lio-uring` exposes io_uring:
//!
//! - [`Queue::push_read`] and [`Queue::push_write`] start an operation on
//!   caller memory and tag it with `user_data`.
//! - [`Queue::push_call`] starts a WASI call such as [`fs::OpenAt`], whose
//!   arguments and result live in the caller's operation value.
//! - [`Queue::wait`] and [`Queue::poll`] return a [`Completion`] carrying
//!   that `user_data` and how many bytes moved.
//! - [`Queue::cancel`] ends an operation early by its `user_data`.
//!
//! No future is polled and no buffer is copied inside the guest.
//!
//! ## Basic usage
//!
//! ```rust,ignore
//! use lio_wasi::{Outcome, Queue};
//!
//! let (stdin, _result) = wasip3::cli::stdin::read_via_stream();
//! let mut queue = Queue::new();
//! let stdin = queue.add_reader(stdin);
//!
//! let mut buf = [0u8; 4096];
//! // SAFETY: `buf` outlives the completion taken just below.
//! unsafe { queue.push_read(stdin, buf.as_mut_ptr(), buf.len(), 7) }?;
//! let done = queue.wait().unwrap();
//! assert_eq!(done.user_data(), 7);
//! ```
//!
//! ## Safety
//!
//! [`Queue::push_read`], [`Queue::push_write`] and [`Queue::push_call`] are
//! `unsafe` because the host uses the buffer or operation value after the
//! call returns. The caller keeps it allocated, unmoved, and otherwise
//! untouched until the operation's [`Completion`] has been returned.
//!
//! ## Target
//!
//! Build for `wasm32-wasip3`. [`Queue::wait`] blocks the calling task, and
//! the component model lets only an async export do that. That target's
//! `main` is one. On `wasm32-wasip2` `main` is a synchronous export, and the
//! first blocking wait traps.
//!
//! ## Status
//!
//! Bound: byte-stream reads and writes, their completion status, timers,
//! and the `wasi:filesystem` and `wasi:sockets` calls that correspond to an
//! io_uring operation. See the crate README for the full table.

#![deny(clippy::unwrap_used, clippy::expect_used)]

use std::collections::VecDeque;
use std::fmt;

use wasip3::wit_bindgen::{StreamReader, StreamWriter};
use wasip3::wit_stream::StreamPayload;

use fs::{DirEntryBuf, DirStream, DirectoryEntry};
use sockets::{Accepted, Listener, TcpSocket};

/// The `wasip3` bindings this crate is built on. Use them for the WASI
/// functions that do not wait and so have no operation here.
pub use wasip3;

pub mod clock;
pub mod fs;
mod reply;
pub mod sockets;
mod sys;

use sys::{Event, WaitableSet};

/// A WASI call that [`Queue::push_call`] can start.
///
/// Implemented by the operation types in this crate, such as
/// [`fs::OpenAt`]. It cannot be implemented outside it.
pub trait Call: sealed::Sealed {}

mod sealed {
  /// What the host answered when an operation started.
  pub enum Started {
    /// An async import was called. This is its packed state and subtask
    /// handle.
    Subtask(u32),
    /// A `future` is being read. `code` is the read's result code.
    Future {
      handle: u32,
      code: u32,
      /// The future type's `cancel-read` built-in.
      cancel: unsafe extern "C" fn(u32) -> u32,
    },
    /// Nothing reached the host; the operation is already done.
    Done,
  }

  pub trait Sealed {
    /// Starts the operation with this value's argument and result storage.
    ///
    /// # Safety
    ///
    /// The operation is not in flight. It stays allocated, unmoved and
    /// untouched until it completes or is cancelled.
    unsafe fn start(&mut self) -> Started;
  }
}

use sealed::Started;

/// An operation that does nothing and completes at once.
#[derive(Clone, Copy, Debug, Default)]
pub struct Nop;

impl Call for Nop {}

impl sealed::Sealed for Nop {
  unsafe fn start(&mut self) -> Started {
    Started::Done
  }
}

/// How an operation ended. For a stream operation, also how many items it
/// moved first: bytes, accepted sockets, or directory entries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
  /// The operation moved this many items and the stream is still open.
  Transferred(usize),
  /// The other end closed the stream. This many items were moved first.
  /// Later operations on it complete at once with `Closed(0)`.
  Closed(usize),
  /// The operation was cancelled. This many items were moved first. A
  /// cancelled call has no result.
  Cancelled(usize),
  /// The call returned. Its result is in the operation value.
  Returned,
}

impl Outcome {
  /// Items moved before the operation ended.
  pub fn count(&self) -> usize {
    match *self {
      Outcome::Transferred(n) | Outcome::Closed(n) | Outcome::Cancelled(n) => n,
      Outcome::Returned => 0,
    }
  }

  /// Decodes a stream result code. `BLOCKED` is not a result.
  fn decode(code: u32) -> Option<Outcome> {
    if code == sys::BLOCKED {
      return None;
    }
    // A count is at most 28 bits, so it fits a `usize` on every target.
    let count = (code >> 4) as usize;
    Some(match code & 0xf {
      sys::COMPLETED => Outcome::Transferred(count),
      sys::DROPPED => Outcome::Closed(count),
      sys::CANCELLED => Outcome::Cancelled(count),
      other => unreachable!("unknown stream result code {other:#x}"),
    })
  }

  /// Decodes the result code of a `future` read, which moves one value or
  /// none. `BLOCKED` is not a result.
  fn decode_future(code: u32) -> Option<Outcome> {
    Some(match Outcome::decode(code)? {
      Outcome::Transferred(_) => Outcome::Returned,
      Outcome::Closed(_) => Outcome::Closed(0),
      other => other,
    })
  }
}

/// A finished operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Completion {
  user_data: u64,
  outcome: Outcome,
}

impl Completion {
  /// The value passed when the operation was pushed.
  pub fn user_data(&self) -> u64 {
    self.user_data
  }

  /// How the operation ended.
  pub fn outcome(&self) -> Outcome {
    self.outcome
  }
}

/// Why a [`Queue`] call was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
  /// The stream was never added to this queue, or has been closed.
  UnknownStream,
  /// The stream already has an operation in flight. The component model
  /// allows one at a time per stream end.
  Busy,
}

impl fmt::Display for Error {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str(match self {
      Error::UnknownStream => "stream is not open in this queue",
      Error::Busy => "stream already has an operation in flight",
    })
  }
}

impl std::error::Error for Error {}

/// The readable end of a byte stream owned by a [`Queue`].
///
/// Returned by [`Queue::add_reader`]. It stays valid until
/// [`Queue::close_reader`] or until the queue is dropped.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ReadStream(u32);

/// The writable end of a byte stream owned by a [`Queue`].
///
/// Returned by [`Queue::add_writer`]. It stays valid until
/// [`Queue::close_writer`] or until the queue is dropped.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct WriteStream(u32);

/// Which end of a stream a handle is, and of what.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum End {
  /// The readable end of a `stream<u8>`.
  Readable,
  /// The writable end of a `stream<u8>`.
  Writable,
  /// The readable end of a listener's `stream<tcp-socket>`.
  Accepting,
  /// The readable end of a directory's `stream<directory-entry>`.
  Listing,
}

impl End {
  /// The waitable-set event that ends a blocked operation on this end.
  fn event(self) -> u32 {
    match self {
      End::Readable | End::Accepting | End::Listing => sys::EVENT_STREAM_READ,
      End::Writable => sys::EVENT_STREAM_WRITE,
    }
  }
}

/// What the queue tracks for one stream end it owns.
#[derive(Clone, Copy, Debug)]
struct Slot {
  end: End,
  /// `user_data` of the blocked operation, if one is in flight.
  pending: Option<u64>,
  /// The other end closed. The component model traps on another operation
  /// after that, so the queue answers `Closed(0)` itself.
  closed: bool,
  /// The built-in that cancels this end's operation.
  cancel: unsafe extern "C" fn(u32) -> u32,
  /// The built-in that drops this end.
  drop: unsafe extern "C" fn(u32),
}

/// A `future` read in flight.
#[derive(Clone, Copy, Debug)]
struct FutureRead {
  user_data: u64,
  cancel: unsafe extern "C" fn(u32) -> u32,
}

/// A completion queue for WASI 0.3 operations.
///
/// The queue owns the stream ends added to it and one `waitable-set`. It is
/// not `Send`: handles belong to the component instance's single thread.
pub struct Queue {
  /// Indexed by stream handle. Handles are small indices into a table.
  streams: Vec<Option<Slot>>,
  /// `user_data` of each call in flight, indexed by subtask handle.
  ///
  /// Calls, futures and streams are kept in separate tables and told apart
  /// by the kind of event, so nothing relies on them sharing a handle table.
  calls: Vec<Option<u64>>,
  /// Future reads in flight, indexed by future handle.
  futures: Vec<Option<FutureRead>>,
  /// Operations whose result arrives through the waitable set.
  in_flight: usize,
  /// Results that were known without waiting.
  ready: VecDeque<Completion>,
  /// Declared last: every waitable must leave the set before it is dropped.
  set: WaitableSet,
}

impl Queue {
  /// Creates an empty queue.
  pub fn new() -> Self {
    Self::with_capacity(0)
  }

  /// Creates a queue with room for `cap` immediate completions.
  pub fn with_capacity(cap: usize) -> Self {
    Self {
      streams: Vec::new(),
      calls: Vec::new(),
      futures: Vec::new(),
      in_flight: 0,
      ready: VecDeque::with_capacity(cap),
      set: WaitableSet::new(),
    }
  }

  /// Takes ownership of the readable end of a byte stream.
  pub fn add_reader(&mut self, reader: StreamReader<u8>) -> ReadStream {
    let handle = reader.take_handle();
    let ops = sys::byte_stream();
    self.adopt(handle, End::Readable, ops.cancel_read, ops.drop_readable);
    ReadStream(handle)
  }

  /// Takes ownership of the writable end of a byte stream.
  pub fn add_writer(&mut self, writer: StreamWriter<u8>) -> WriteStream {
    let handle = writer.handle();
    // The writer has no way to give up its handle. Forgetting it skips the
    // destructor that would drop the handle; it owns no heap memory.
    std::mem::forget(writer);
    let ops = sys::byte_stream();
    self.adopt(handle, End::Writable, ops.cancel_write, ops.drop_writable);
    WriteStream(handle)
  }

  /// Starts reading up to `len` bytes into `buf`.
  ///
  /// The read's [`Completion`] carries `user_data`. It comes from a later
  /// [`wait`](Self::wait) or [`poll`](Self::poll), also when the read
  /// finished at once.
  ///
  /// # Safety
  ///
  /// `buf` is valid for writes of `len` bytes. It stays allocated and
  /// unmoved, and nothing else reads or writes it, until this read's
  /// completion has been returned.
  pub unsafe fn push_read(
    &mut self,
    stream: ReadStream,
    buf: *mut u8,
    len: usize,
    user_data: u64,
  ) -> Result<(), Error> {
    if !self.begin(stream.0, End::Readable, user_data)? {
      return Ok(());
    }
    let len = len.min(sys::MAX_LENGTH);
    // SAFETY: the handle is an open readable end with no read in flight, and
    // the caller vouches for `buf`.
    let code = unsafe { (sys::byte_stream().start_read)(stream.0, buf, len) };
    self.started(stream.0, user_data, code);
    Ok(())
  }

  /// Starts writing up to `len` bytes from `buf`.
  ///
  /// One write may move fewer than `len` bytes. The write's [`Completion`]
  /// carries `user_data` and the count. It comes from a later
  /// [`wait`](Self::wait) or [`poll`](Self::poll), also when the write
  /// finished at once.
  ///
  /// # Safety
  ///
  /// `buf` is valid for reads of `len` bytes. It stays allocated and
  /// unmoved, and nothing writes it, until this write's completion has been
  /// returned.
  pub unsafe fn push_write(
    &mut self,
    stream: WriteStream,
    buf: *const u8,
    len: usize,
    user_data: u64,
  ) -> Result<(), Error> {
    if !self.begin(stream.0, End::Writable, user_data)? {
      return Ok(());
    }
    let len = len.min(sys::MAX_LENGTH);
    // SAFETY: the handle is an open writable end with no write in flight,
    // and the caller vouches for `buf`.
    let code = unsafe { (sys::byte_stream().start_write)(stream.0, buf, len) };
    self.started(stream.0, user_data, code);
    Ok(())
  }

  /// Takes ownership of a listening socket's stream of connections, as
  /// returned by `TcpSocket::listen`.
  pub fn add_listener(
    &mut self,
    connections: StreamReader<TcpSocket>,
  ) -> Listener {
    let handle = connections.take_handle();
    let ops = <TcpSocket as StreamPayload>::VTABLE;
    self.adopt(handle, End::Accepting, ops.cancel_read, ops.drop_readable);
    Listener(handle)
  }

  /// Starts accepting one connection into `slot`, like `accept`.
  ///
  /// The accept's [`Completion`] carries `user_data`. It reports
  /// [`Outcome::Transferred(1)`](Outcome::Transferred) when a connection
  /// arrived; take it with [`Accepted::socket`].
  ///
  /// # Safety
  ///
  /// `slot` stays allocated and unmoved, and nothing else reads or writes
  /// it, until this accept's completion has been returned.
  pub unsafe fn push_accept(
    &mut self,
    listener: Listener,
    slot: &mut Accepted,
    user_data: u64,
  ) -> Result<(), Error> {
    if !self.begin(listener.0, End::Accepting, user_data)? {
      return Ok(());
    }
    let ops = <TcpSocket as StreamPayload>::VTABLE;
    // SAFETY: the handle is an open listener with no accept in flight, and
    // the caller vouches for `slot`, which holds one socket handle.
    let code = unsafe { (ops.start_read)(listener.0, slot.arm(), 1) };
    self.started(listener.0, user_data, code);
    Ok(())
  }

  /// Stops listening, cancelling the accept in flight if there is one.
  ///
  /// A cancelled accept still produces its completion.
  pub fn close_listener(&mut self, listener: Listener) -> Result<(), Error> {
    self.close(listener.0, End::Accepting)
  }

  /// Takes ownership of a directory's stream of entries, as returned by
  /// `Descriptor::read_directory`.
  pub fn add_dir_reader(
    &mut self,
    entries: StreamReader<DirectoryEntry>,
  ) -> DirStream {
    let handle = entries.take_handle();
    let ops = <DirectoryEntry as StreamPayload>::VTABLE;
    self.adopt(handle, End::Listing, ops.cancel_read, ops.drop_readable);
    DirStream(handle)
  }

  /// Starts reading directory entries into `buf`, up to its capacity.
  ///
  /// The read's [`Completion`] carries `user_data` and reports how many
  /// entries arrived; take them with [`DirEntryBuf::drain`].
  /// [`Outcome::Closed`] means the listing is over, after the entries it
  /// counts.
  ///
  /// # Safety
  ///
  /// `buf` stays allocated and unmoved, and nothing else reads or writes
  /// it, until this read's completion has been returned.
  pub unsafe fn push_read_dir(
    &mut self,
    stream: DirStream,
    buf: &mut DirEntryBuf,
    user_data: u64,
  ) -> Result<(), Error> {
    if !self.begin(stream.0, End::Listing, user_data)? {
      return Ok(());
    }
    let (slots, capacity) = buf.arm();
    let capacity = capacity.min(sys::MAX_LENGTH);
    let ops = <DirectoryEntry as StreamPayload>::VTABLE;
    // SAFETY: the handle is an open directory stream with no read in
    // flight, and the caller vouches for `buf`, which holds `capacity`
    // entries.
    let code = unsafe { (ops.start_read)(stream.0, slots, capacity) };
    self.started(stream.0, user_data, code);
    Ok(())
  }

  /// Stops listing, cancelling the read in flight if there is one.
  ///
  /// A cancelled read still produces its completion.
  pub fn close_dir_reader(&mut self, stream: DirStream) -> Result<(), Error> {
    self.close(stream.0, End::Listing)
  }

  /// Starts a WASI call.
  ///
  /// The call's [`Completion`] carries `user_data` and comes from a later
  /// [`wait`](Self::wait) or [`poll`](Self::poll), also when the call
  /// returned at once. After [`Outcome::Returned`] the result is read from
  /// `call` itself.
  ///
  /// # Safety
  ///
  /// `call` is not already in flight. It stays allocated and unmoved, and
  /// nothing else reads or writes it, until this call's completion has been
  /// returned. What it borrows (a path, a descriptor) outlives that too.
  pub unsafe fn push_call<C: Call>(&mut self, call: &mut C, user_data: u64) {
    // SAFETY: the caller vouches for `call`.
    match unsafe { sealed::Sealed::start(call) } {
      Started::Done => self.complete(user_data, Outcome::Returned),
      Started::Subtask(packed) => self.started_call(packed, user_data),
      Started::Future { handle, code, cancel } => {
        match Outcome::decode_future(code) {
          Some(outcome) => self.complete(user_data, outcome),
          None => {
            let read = FutureRead { user_data, cancel };
            set_slot(&mut self.futures, handle, read);
            self.set.join(handle);
            self.in_flight += 1;
          }
        }
      }
    }
  }

  /// Cancels the blocked operation that was pushed with `user_data`.
  ///
  /// Returns whether one was found. Its completion is queued before this
  /// returns: [`Outcome::Cancelled`], or the operation's own outcome if it
  /// finished first. The host no longer uses the operation's memory.
  ///
  /// An operation that already finished is not found; its completion is
  /// already queued or returned.
  pub fn cancel(&mut self, user_data: u64) -> bool {
    // Each index below came from a `u32` handle.
    let stream = self.streams.iter().position(|slot| {
      slot.is_some_and(|slot| slot.pending == Some(user_data))
    });
    if let Some(index) = stream {
      return self.cancel_stream(index as u32);
    }
    let call = self.calls.iter().position(|call| *call == Some(user_data));
    if let Some(index) = call {
      return self.cancel_call(index as u32);
    }
    let future = self
      .futures
      .iter()
      .position(|read| read.is_some_and(|read| read.user_data == user_data));
    future.is_some_and(|index| self.cancel_future(index as u32))
  }

  /// Closes the readable end, cancelling its read first if one is in flight.
  ///
  /// A cancelled read still produces its completion. The writer on the other
  /// end sees the stream closed.
  pub fn close_reader(&mut self, stream: ReadStream) -> Result<(), Error> {
    self.close(stream.0, End::Readable)
  }

  /// Closes the writable end, cancelling its write first if one is in
  /// flight.
  ///
  /// A cancelled write still produces its completion. The reader on the
  /// other end sees the stream closed once it has taken what was written.
  pub fn close_writer(&mut self, stream: WriteStream) -> Result<(), Error> {
    self.close(stream.0, End::Writable)
  }

  /// Operations whose completion has not been returned yet.
  pub fn outstanding(&self) -> usize {
    self.in_flight + self.ready.len()
  }

  /// Returns the next completion, blocking until one arrives.
  ///
  /// Returns `None` when nothing is outstanding.
  pub fn wait(&mut self) -> Option<Completion> {
    if let Some(done) = self.ready.pop_front() {
      return Some(done);
    }
    // Every pass handles one event. An event that is not a completion (a
    // call that only started) leaves its operation in flight, so the next
    // wait still has something to deliver.
    loop {
      if self.in_flight == 0 {
        return None;
      }
      let event = self.set.wait();
      if let Some(done) = self.finish(event) {
        return Some(done);
      }
    }
  }

  /// Returns the next completion if one is ready, without blocking.
  pub fn poll(&mut self) -> Option<Completion> {
    if let Some(done) = self.ready.pop_front() {
      return Some(done);
    }
    loop {
      if self.in_flight == 0 {
        return None;
      }
      let event = self.set.poll()?;
      if let Some(done) = self.finish(event) {
        return Some(done);
      }
    }
  }

  fn adopt(
    &mut self,
    handle: u32,
    end: End,
    cancel: unsafe extern "C" fn(u32) -> u32,
    drop: unsafe extern "C" fn(u32),
  ) {
    let slot = Slot { end, pending: None, closed: false, cancel, drop };
    set_slot(&mut self.streams, handle, slot);
  }

  /// The slot for `handle`, if the queue owns it as an `end`.
  fn slot_mut(&mut self, handle: u32, end: End) -> Result<&mut Slot, Error> {
    self
      .streams
      .get_mut(handle as usize)
      .and_then(Option::as_mut)
      .filter(|slot| slot.end == end)
      .ok_or(Error::UnknownStream)
  }

  /// Queues a completion that needs no waiting.
  fn complete(&mut self, user_data: u64, outcome: Outcome) {
    self.ready.push_back(Completion { user_data, outcome });
  }

  /// Checks that an operation may start on `handle`.
  ///
  /// Returns `false` when the stream is closed: the operation is answered
  /// here with `Closed(0)` and must not reach the host.
  fn begin(
    &mut self,
    handle: u32,
    end: End,
    user_data: u64,
  ) -> Result<bool, Error> {
    let slot = self.slot_mut(handle, end)?;
    if slot.pending.is_some() {
      return Err(Error::Busy);
    }
    if slot.closed {
      self.complete(user_data, Outcome::Closed(0));
      return Ok(false);
    }
    Ok(true)
  }

  /// Records what the host answered when a stream operation started.
  fn started(&mut self, handle: u32, user_data: u64, code: u32) {
    let Some(Some(slot)) = self.streams.get_mut(handle as usize) else {
      unreachable!("an operation started on a stream with no slot")
    };
    match Outcome::decode(code) {
      Some(outcome) => {
        slot.closed |= matches!(outcome, Outcome::Closed(_));
        self.complete(user_data, outcome);
      }
      None => {
        slot.pending = Some(user_data);
        self.set.join(handle);
        self.in_flight += 1;
      }
    }
  }

  /// Records what the host answered when an async import was called.
  fn started_call(&mut self, packed: u32, user_data: u64) {
    let subtask = packed >> 4;
    match packed & 0xf {
      sys::STATUS_RETURNED => {
        if subtask != 0 {
          sys::subtask_drop(subtask);
        }
        self.complete(user_data, Outcome::Returned);
      }
      sys::STATUS_STARTING | sys::STATUS_STARTED => {
        set_slot(&mut self.calls, subtask, user_data);
        self.set.join(subtask);
        self.in_flight += 1;
      }
      other => unreachable!("a call started in unknown state {other:#x}"),
    }
  }

  /// Cancels the operation in flight on a stream. Returns whether there was
  /// one.
  fn cancel_stream(&mut self, handle: u32) -> bool {
    let Some(Some(slot)) = self.streams.get_mut(handle as usize) else {
      return false;
    };
    let Some(user_data) = slot.pending.take() else {
      return false;
    };
    // A waitable that is in a set cannot be cancelled synchronously: the
    // host traps. Leaving first is safe, because the cancel below returns
    // the operation's result itself and no event follows.
    WaitableSet::leave(handle);
    // SAFETY: the handle is an open end of this kind with an operation in
    // flight.
    let code = unsafe { (slot.cancel)(handle) };
    let Some(outcome) = Outcome::decode(code) else {
      unreachable!("a synchronous stream cancel returned BLOCKED")
    };
    slot.closed |= matches!(outcome, Outcome::Closed(_));
    self.in_flight -= 1;
    self.complete(user_data, outcome);
    true
  }

  /// Cancels the call in flight on `subtask`. Returns whether there was
  /// one.
  fn cancel_call(&mut self, subtask: u32) -> bool {
    let Some(user_data) =
      self.calls.get_mut(subtask as usize).and_then(Option::take)
    else {
      return false;
    };
    // As for streams: leave the set before cancelling synchronously.
    WaitableSet::leave(subtask);
    let outcome = match sys::subtask_cancel(subtask) {
      sys::STATUS_RETURNED => Outcome::Returned,
      sys::STATUS_STARTED_CANCELLED | sys::STATUS_RETURNED_CANCELLED => {
        Outcome::Cancelled(0)
      }
      other => unreachable!("a cancelled call ended in state {other:#x}"),
    };
    sys::subtask_drop(subtask);
    self.in_flight -= 1;
    self.complete(user_data, outcome);
    true
  }

  /// Cancels the read in flight on a future. Returns whether there was one.
  fn cancel_future(&mut self, handle: u32) -> bool {
    let Some(read) =
      self.futures.get_mut(handle as usize).and_then(Option::take)
    else {
      return false;
    };
    // As for streams: leave the set before cancelling synchronously.
    WaitableSet::leave(handle);
    // SAFETY: the handle is a future's readable end with a read in flight.
    let code = unsafe { (read.cancel)(handle) };
    let Some(outcome) = Outcome::decode_future(code) else {
      unreachable!("a synchronous future cancel returned BLOCKED")
    };
    self.in_flight -= 1;
    self.complete(read.user_data, outcome);
    true
  }

  fn close(&mut self, handle: u32, end: End) -> Result<(), Error> {
    let drop = self.slot_mut(handle, end)?.drop;
    self.cancel_stream(handle);
    self.streams[handle as usize] = None;
    // SAFETY: the handle is an open end of this kind with nothing in flight,
    // and its slot is gone, so it is dropped once.
    unsafe { drop(handle) };
    Ok(())
  }

  /// Handles one waitable-set event. Returns the completion of the
  /// operation it ends, or `None` when the operation is still in flight.
  fn finish(&mut self, event: Event) -> Option<Completion> {
    match event.kind {
      sys::EVENT_SUBTASK => self.finish_call(event),
      sys::EVENT_FUTURE_READ => Some(self.finish_future(event)),
      _ => Some(self.finish_stream(event)),
    }
  }

  /// Handles a state change of a call.
  fn finish_call(&mut self, event: Event) -> Option<Completion> {
    let Some(slot) = self.calls.get_mut(event.waitable as usize) else {
      unreachable!("event for a call this queue did not start: {event:?}")
    };
    match event.code {
      sys::STATUS_STARTING | sys::STATUS_STARTED => None,
      sys::STATUS_RETURNED => {
        let Some(user_data) = slot.take() else {
          unreachable!("event for a call that is not in flight: {event:?}")
        };
        WaitableSet::leave(event.waitable);
        sys::subtask_drop(event.waitable);
        self.in_flight -= 1;
        Some(Completion { user_data, outcome: Outcome::Returned })
      }
      other => unreachable!("a call moved to unexpected state {other:#x}"),
    }
  }

  /// Turns a future event into the completion of its read.
  fn finish_future(&mut self, event: Event) -> Completion {
    let Some(read) =
      self.futures.get_mut(event.waitable as usize).and_then(Option::take)
    else {
      unreachable!("event for a future with no read in flight: {event:?}")
    };
    let Some(outcome) = Outcome::decode_future(event.code) else {
      unreachable!("a future event carried BLOCKED")
    };
    WaitableSet::leave(event.waitable);
    self.in_flight -= 1;
    Completion { user_data: read.user_data, outcome }
  }

  /// Turns a stream event into the completion of the operation it ends.
  fn finish_stream(&mut self, event: Event) -> Completion {
    let Some(Some(slot)) = self.streams.get_mut(event.waitable as usize) else {
      unreachable!("event for a stream this queue does not own: {event:?}")
    };
    if event.kind != slot.end.event() {
      unreachable!("event does not match its stream end: {event:?}")
    }
    let Some(user_data) = slot.pending.take() else {
      unreachable!("event for a stream with nothing in flight: {event:?}")
    };
    let Some(outcome) = Outcome::decode(event.code) else {
      unreachable!("a stream event carried BLOCKED")
    };
    slot.closed |= matches!(outcome, Outcome::Closed(_));
    WaitableSet::leave(event.waitable);
    self.in_flight -= 1;
    Completion { user_data, outcome }
  }
}

/// Stores `value` at `handle`, growing the table to reach it.
fn set_slot<T: Clone>(table: &mut Vec<Option<T>>, handle: u32, value: T) {
  let index = handle as usize;
  if table.len() <= index {
    table.resize(index + 1, None);
  }
  table[index] = Some(value);
}

impl Default for Queue {
  fn default() -> Self {
    Self::new()
  }
}

impl Drop for Queue {
  fn drop(&mut self) {
    // Stop every operation so the host stops using caller memory, then drop
    // the stream ends. The set drops after this, once it is empty. Each
    // index came from a `u32` handle.
    for index in 0..self.calls.len() {
      self.cancel_call(index as u32);
    }
    for index in 0..self.futures.len() {
      self.cancel_future(index as u32);
    }
    for index in 0..self.streams.len() {
      if let Some(slot) = self.streams[index] {
        let _ = self.close(index as u32, slot.end);
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn decode_splits_count_from_kind() {
    assert_eq!(Outcome::decode(5 << 4), Some(Outcome::Transferred(5)));
    assert_eq!(Outcome::decode((3 << 4) | 1), Some(Outcome::Closed(3)));
    assert_eq!(Outcome::decode(2), Some(Outcome::Cancelled(0)));
  }

  #[test]
  fn blocked_is_not_an_outcome() {
    assert_eq!(Outcome::decode(sys::BLOCKED), None);
    assert_eq!(Outcome::decode_future(sys::BLOCKED), None);
  }

  #[test]
  fn a_future_that_delivers_has_returned() {
    assert_eq!(Outcome::decode_future(0), Some(Outcome::Returned));
    assert_eq!(Outcome::decode_future(2), Some(Outcome::Cancelled(0)));
    assert_eq!(Outcome::decode_future(1), Some(Outcome::Closed(0)));
  }

  #[test]
  fn count_reports_items_for_every_outcome() {
    assert_eq!(Outcome::Transferred(7).count(), 7);
    assert_eq!(Outcome::Closed(2).count(), 2);
    assert_eq!(Outcome::Cancelled(1).count(), 1);
    assert_eq!(Outcome::Returned.count(), 0);
  }

  #[test]
  fn each_end_expects_its_own_event() {
    assert_eq!(End::Readable.event(), sys::EVENT_STREAM_READ);
    assert_eq!(End::Accepting.event(), sys::EVENT_STREAM_READ);
    assert_eq!(End::Listing.event(), sys::EVENT_STREAM_READ);
    assert_eq!(End::Writable.event(), sys::EVENT_STREAM_WRITE);
  }

  #[test]
  fn set_slot_grows_the_table_to_the_handle() {
    let mut table: Vec<Option<u64>> = Vec::new();
    set_slot(&mut table, 3, 9);
    assert_eq!(table, [None, None, None, Some(9)]);
    set_slot(&mut table, 1, 4);
    assert_eq!(table, [None, Some(4), None, Some(9)]);
  }
}
