//! # lio-wasi
//!
//! WASI 0.3 without an executor: operations started and awaited through the
//! component model's async built-ins.
//!
//! `wasip3` exposes WASI 0.3 as Rust futures and streams that read into an
//! owned `Vec`. Underneath, the component model works differently:
//! `stream.read` takes a pointer and a length, an async import returns a
//! subtask, and a `waitable-set` reports which blocked operation finished.
//! This crate exposes that layer directly, through a [`WaitableSet`]:
//!
//! - [`WaitableSet::read`] and [`WaitableSet::write`] move bytes between a
//!   stream and caller memory. They answer at once with a [`StreamResult`],
//!   or with [`Started::Blocked`].
//! - [`WaitableSet::start`] calls a WASI function such as [`fs::OpenAt`],
//!   whose arguments and result live in the caller's value. It returns the
//!   [`Subtask`] to wait for, or `None` if the call already returned.
//! - [`WaitableSet::wait`] and [`WaitableSet::poll`] return an [`Event`]
//!   naming the stream or subtask whose blocked operation ended.
//! - Each kind of operation has its cancel, which answers at once.
//!
//! An operation is identified by what the component model itself waits on:
//! the stream it runs on, or the subtask of the call. No future is polled
//! and no buffer is copied inside the guest.
//!
//! ## Basic usage
//!
//! ```rust,ignore
//! use lio_wasi::{Event, Started, WaitableSet};
//!
//! let (stdin, _result) = wasip3::cli::stdin::read_via_stream();
//! let mut set = WaitableSet::new();
//! let stdin = set.add_reader(stdin);
//!
//! let mut buf = [0u8; 4096];
//! // SAFETY: `buf` outlives the read, which has ended once this block has.
//! let read = match unsafe { set.read(stdin, buf.as_mut_ptr(), buf.len()) }? {
//!   Started::Done(read) => read,
//!   Started::Blocked => match set.wait() {
//!     Some(Event::Read(stream, read)) if stream == stdin => read,
//!     other => unreachable!("only the read is in flight: {other:?}"),
//!   },
//! };
//! println!("{} bytes", read.count());
//! ```
//!
//! ## Safety
//!
//! Starting an operation is `unsafe` because the host may use the buffer or
//! operation value after the call returns. If the operation blocked, the
//! caller keeps that memory allocated, unmoved, and otherwise untouched
//! until an [`Event`] or a cancel reports the operation's end.
//!
//! ## Target
//!
//! Build for `wasm32-wasip3`. [`WaitableSet::wait`] blocks the calling task,
//! and the component model lets only an async export do that. That target's
//! `main` is one. On `wasm32-wasip2` `main` is a synchronous export, and the
//! first blocking wait traps.
//!
//! ## Status
//!
//! Bound: byte-stream reads and writes, the status of their streams, and the
//! `wasi:clocks`, `wasi:filesystem` and `wasi:sockets` functions that wait.
//! See the crate README for the full table.

#![deny(clippy::unwrap_used, clippy::expect_used)]

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

use sys::{RawEvent, RawSet};

/// A WASI function call that [`WaitableSet::start`] can make.
///
/// Implemented by the operation types in this crate, such as
/// [`fs::OpenAt`]. It cannot be implemented outside it.
pub trait Call: sealed::Call {}

/// A `future` that [`WaitableSet::read_future`] can read: the status of a
/// stream, [`fs::StreamStatus`] or [`sockets::StreamStatus`].
///
/// It cannot be implemented outside this crate.
pub trait ReadableFuture: sealed::Future {}

mod sealed {
  pub trait Call {
    /// Calls the async import with this value's argument and result
    /// storage. Returns the packed state and subtask handle.
    ///
    /// # Safety
    ///
    /// The call is not in flight. The value stays allocated, unmoved and
    /// untouched until the call returns or is cancelled.
    unsafe fn start(&mut self) -> u32;
  }

  /// What the host answered when a future read started.
  pub struct FutureStart {
    pub handle: u32,
    /// The read's result code.
    pub code: u32,
    /// The future type's `cancel-read` built-in.
    pub cancel: unsafe extern "C" fn(u32) -> u32,
  }

  pub trait Future {
    /// Starts reading the future's value into this value's storage.
    ///
    /// # Safety
    ///
    /// No read is in flight. The value stays allocated, unmoved and
    /// untouched until the read completes or is cancelled.
    unsafe fn start_read(&mut self) -> FutureStart;
  }
}

/// A call to a WASI function that has not returned.
///
/// Returned by [`WaitableSet::start`]. It names the call until
/// [`Event::Returned`] or [`WaitableSet::cancel`] reports its end; the host
/// may then give the same value to a later call.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Subtask(u32);

/// The readable end of a `future` with a read in flight.
///
/// Returned by [`WaitableSet::read_future`]. It names the read until
/// [`Event::FutureRead`] or [`WaitableSet::cancel_future`] reports its end.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ReadFuture(u32);

/// How a stream operation ended, and how many items it moved first: bytes,
/// accepted sockets, or directory entries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamResult {
  /// The operation moved this many items and the stream is still open.
  Completed(usize),
  /// The other end was dropped. This many items were moved first. Later
  /// operations on the stream answer `Dropped(0)` at once.
  Dropped(usize),
  /// The operation was cancelled. This many items were moved first.
  Cancelled(usize),
}

impl StreamResult {
  /// Items moved before the operation ended.
  pub fn count(&self) -> usize {
    match *self {
      StreamResult::Completed(n)
      | StreamResult::Dropped(n)
      | StreamResult::Cancelled(n) => n,
    }
  }

  /// Decodes a stream result code. `BLOCKED` is not a result.
  fn decode(code: u32) -> Option<StreamResult> {
    if code == sys::BLOCKED {
      return None;
    }
    // A count is at most 28 bits, so it fits a `usize` on every target.
    let count = (code >> 4) as usize;
    Some(match code & 0xf {
      sys::COMPLETED => StreamResult::Completed(count),
      sys::DROPPED => StreamResult::Dropped(count),
      sys::CANCELLED => StreamResult::Cancelled(count),
      other => unreachable!("unknown stream result code {other:#x}"),
    })
  }
}

/// What the host answered when a stream operation started.
#[must_use = "a blocked operation ends in an event from the set"]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Started {
  /// The operation is over. The host no longer uses its memory.
  Done(StreamResult),
  /// The operation is in flight. An [`Event`] from the set, or the stream's
  /// cancel, reports its end.
  Blocked,
}

/// What cancelling a call or a future read found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cancel {
  /// It finished before the cancel reached it. Its result is in the
  /// operation value.
  Completed,
  /// It was stopped. There is no result.
  Cancelled,
}

impl Cancel {
  /// Decodes the result code of a `future` read, which delivers its one
  /// value or is cancelled. `BLOCKED` is not a result.
  fn decode_future(code: u32) -> Option<Cancel> {
    if code == sys::BLOCKED {
      return None;
    }
    Some(match code & 0xf {
      sys::COMPLETED => Cancel::Completed,
      sys::CANCELLED => Cancel::Cancelled,
      other => unreachable!("unknown future result code {other:#x}"),
    })
  }
}

/// The end of an operation that had blocked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
  /// A call returned. Its result is in the operation value.
  Returned(Subtask),
  /// A future delivered its value into the value it was read through.
  FutureRead(ReadFuture),
  /// A [`read`](WaitableSet::read) ended.
  Read(ReadStream, StreamResult),
  /// A [`write`](WaitableSet::write) ended.
  Write(WriteStream, StreamResult),
  /// An [`accept`](WaitableSet::accept) ended.
  Accept(Listener, StreamResult),
  /// A [`read_dir`](WaitableSet::read_dir) ended.
  ReadDir(DirStream, StreamResult),
}

/// Why a [`WaitableSet`] call was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
  /// The stream was never added to this set, or has been closed.
  UnknownStream,
  /// The stream already has an operation in flight. The component model
  /// allows one at a time per stream end.
  Busy,
}

impl fmt::Display for Error {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str(match self {
      Error::UnknownStream => "stream is not open in this set",
      Error::Busy => "stream already has an operation in flight",
    })
  }
}

impl std::error::Error for Error {}

/// The readable end of a byte stream owned by a [`WaitableSet`].
///
/// Returned by [`WaitableSet::add_reader`]. It stays valid until
/// [`WaitableSet::close_reader`] or until the set is dropped.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ReadStream(u32);

/// The writable end of a byte stream owned by a [`WaitableSet`].
///
/// Returned by [`WaitableSet::add_writer`]. It stays valid until
/// [`WaitableSet::close_writer`] or until the set is dropped.
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

/// What the set tracks for one stream end it owns.
#[derive(Clone, Copy, Debug)]
struct Slot {
  end: End,
  /// An operation is in flight.
  blocked: bool,
  /// The other end was dropped. The component model traps on another
  /// operation after that, so the set answers `Dropped(0)` itself.
  dropped: bool,
  /// The built-in that cancels this end's operation.
  cancel: unsafe extern "C" fn(u32) -> u32,
  /// The built-in that drops this end.
  drop: unsafe extern "C" fn(u32),
}

/// A component-model `waitable-set`, the stream ends it owns, and the calls
/// and future reads in flight on it.
///
/// It is not `Send`: handles belong to the component instance's single
/// thread.
pub struct WaitableSet {
  /// Indexed by stream handle. Handles are small indices into a table.
  streams: Vec<Option<Slot>>,
  /// Whether a call is in flight, indexed by subtask handle.
  ///
  /// Calls, futures and streams are kept in separate tables and told apart
  /// by the kind of event, so nothing relies on them sharing a handle table.
  calls: Vec<bool>,
  /// The cancel built-in of each future read in flight, indexed by future
  /// handle.
  futures: Vec<Option<unsafe extern "C" fn(u32) -> u32>>,
  /// Operations in flight: each ends in an event or by its cancel.
  in_flight: usize,
  /// Declared last: every waitable must leave the set before it is dropped.
  set: RawSet,
}

impl WaitableSet {
  /// Creates an empty set.
  pub fn new() -> Self {
    Self {
      streams: Vec::new(),
      calls: Vec::new(),
      futures: Vec::new(),
      in_flight: 0,
      set: RawSet::new(),
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

  /// Reads up to `len` bytes into `buf`.
  ///
  /// # Safety
  ///
  /// `buf` is valid for writes of `len` bytes. If the read blocked, `buf`
  /// stays allocated and unmoved, and nothing else reads or writes it,
  /// until [`Event::Read`] or [`cancel_read`](Self::cancel_read) reports
  /// the read's end.
  pub unsafe fn read(
    &mut self,
    stream: ReadStream,
    buf: *mut u8,
    len: usize,
  ) -> Result<Started, Error> {
    if !self.begin(stream.0, End::Readable)? {
      return Ok(Started::Done(StreamResult::Dropped(0)));
    }
    let len = len.min(sys::MAX_LENGTH);
    // SAFETY: the handle is an open readable end with no read in flight, and
    // the caller vouches for `buf`.
    let code = unsafe { (sys::byte_stream().start_read)(stream.0, buf, len) };
    Ok(self.started(stream.0, code))
  }

  /// Writes up to `len` bytes from `buf`. One write may move fewer.
  ///
  /// # Safety
  ///
  /// `buf` is valid for reads of `len` bytes. If the write blocked, `buf`
  /// stays allocated and unmoved, and nothing writes it, until
  /// [`Event::Write`] or [`cancel_write`](Self::cancel_write) reports the
  /// write's end.
  pub unsafe fn write(
    &mut self,
    stream: WriteStream,
    buf: *const u8,
    len: usize,
  ) -> Result<Started, Error> {
    if !self.begin(stream.0, End::Writable)? {
      return Ok(Started::Done(StreamResult::Dropped(0)));
    }
    let len = len.min(sys::MAX_LENGTH);
    // SAFETY: the handle is an open writable end with no write in flight,
    // and the caller vouches for `buf`.
    let code = unsafe { (sys::byte_stream().start_write)(stream.0, buf, len) };
    Ok(self.started(stream.0, code))
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

  /// Accepts one connection into `slot`.
  ///
  /// [`StreamResult::Completed(1)`](StreamResult::Completed) means a
  /// connection arrived; take it with [`Accepted::socket`].
  ///
  /// # Safety
  ///
  /// If the accept blocked, `slot` stays allocated and unmoved, and nothing
  /// else reads or writes it, until [`Event::Accept`] or
  /// [`cancel_accept`](Self::cancel_accept) reports the accept's end.
  pub unsafe fn accept(
    &mut self,
    listener: Listener,
    slot: &mut Accepted,
  ) -> Result<Started, Error> {
    if !self.begin(listener.0, End::Accepting)? {
      return Ok(Started::Done(StreamResult::Dropped(0)));
    }
    let ops = <TcpSocket as StreamPayload>::VTABLE;
    // SAFETY: the handle is an open listener with no accept in flight, and
    // the caller vouches for `slot`, which holds one socket handle.
    let code = unsafe { (ops.start_read)(listener.0, slot.arm(), 1) };
    Ok(self.started(listener.0, code))
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

  /// Reads directory entries into `buf`, up to its capacity.
  ///
  /// The result counts the entries that arrived; take them with
  /// [`DirEntryBuf::drain`]. [`StreamResult::Dropped`] means the listing is
  /// over, after the entries it counts.
  ///
  /// # Safety
  ///
  /// If the read blocked, `buf` stays allocated and unmoved, and nothing
  /// else reads or writes it, until [`Event::ReadDir`] or
  /// [`cancel_read_dir`](Self::cancel_read_dir) reports the read's end.
  pub unsafe fn read_dir(
    &mut self,
    stream: DirStream,
    buf: &mut DirEntryBuf,
  ) -> Result<Started, Error> {
    if !self.begin(stream.0, End::Listing)? {
      return Ok(Started::Done(StreamResult::Dropped(0)));
    }
    let (slots, capacity) = buf.arm();
    let capacity = capacity.min(sys::MAX_LENGTH);
    let ops = <DirectoryEntry as StreamPayload>::VTABLE;
    // SAFETY: the handle is an open directory stream with no read in
    // flight, and the caller vouches for `buf`, which holds `capacity`
    // entries.
    let code = unsafe { (ops.start_read)(stream.0, slots, capacity) };
    Ok(self.started(stream.0, code))
  }

  /// Calls a WASI function.
  ///
  /// Returns `None` if the call already returned: its result is in `call`.
  /// Otherwise returns the [`Subtask`] that [`Event::Returned`] or
  /// [`cancel`](Self::cancel) will name.
  ///
  /// # Safety
  ///
  /// `call` is not already in flight. If the call did not return, `call`
  /// stays allocated and unmoved, and nothing else reads or writes it,
  /// until its subtask's end is reported. What it borrows (a path, a
  /// descriptor) outlives that too.
  #[must_use = "a call that did not return ends in an event from the set"]
  pub unsafe fn start<C: Call>(&mut self, call: &mut C) -> Option<Subtask> {
    // SAFETY: the caller vouches for `call`.
    let packed = unsafe { sealed::Call::start(call) };
    let subtask = packed >> 4;
    match packed & 0xf {
      sys::STATUS_RETURNED => {
        if subtask != 0 {
          sys::subtask_drop(subtask);
        }
        None
      }
      sys::STATUS_STARTING | sys::STATUS_STARTED => {
        set_slot(&mut self.calls, subtask, true);
        self.set.join(subtask);
        self.in_flight += 1;
        Some(Subtask(subtask))
      }
      other => unreachable!("a call started in unknown state {other:#x}"),
    }
  }

  /// Reads a future's value into `future`.
  ///
  /// Returns `None` if the value was already there: it is in `future`.
  /// Otherwise returns the [`ReadFuture`] that [`Event::FutureRead`] or
  /// [`cancel_future`](Self::cancel_future) will name.
  ///
  /// # Safety
  ///
  /// No read of `future` is in flight. If the read blocked, `future` stays
  /// allocated and unmoved, and nothing else reads or writes it, until the
  /// read's end is reported.
  #[must_use = "a blocked read ends in an event from the set"]
  pub unsafe fn read_future<F: ReadableFuture>(
    &mut self,
    future: &mut F,
  ) -> Option<ReadFuture> {
    // SAFETY: the caller vouches for `future`.
    let started = unsafe { sealed::Future::start_read(future) };
    match Cancel::decode_future(started.code) {
      Some(Cancel::Completed) => None,
      Some(Cancel::Cancelled) => {
        unreachable!("a future read was cancelled as it started")
      }
      None => {
        set_slot(&mut self.futures, started.handle, Some(started.cancel));
        self.set.join(started.handle);
        self.in_flight += 1;
        Some(ReadFuture(started.handle))
      }
    }
  }

  /// Cancels a call that has not returned.
  ///
  /// Returns what the cancel found, or `None` if `subtask` names no call in
  /// flight on this set. No event follows for it, and the host no longer
  /// uses the call's memory.
  pub fn cancel(&mut self, subtask: Subtask) -> Option<Cancel> {
    let live = self.calls.get_mut(subtask.0 as usize)?;
    if !std::mem::take(live) {
      return None;
    }
    // A waitable that is in a set cannot be cancelled synchronously: the
    // host traps. Leaving first is safe, because the cancel below returns
    // the call's state itself and no event follows.
    RawSet::leave(subtask.0);
    let found = match sys::subtask_cancel(subtask.0) {
      sys::STATUS_RETURNED => Cancel::Completed,
      sys::STATUS_STARTED_CANCELLED | sys::STATUS_RETURNED_CANCELLED => {
        Cancel::Cancelled
      }
      other => unreachable!("a cancelled call ended in state {other:#x}"),
    };
    sys::subtask_drop(subtask.0);
    self.in_flight -= 1;
    Some(found)
  }

  /// Cancels a future read in flight.
  ///
  /// Returns what the cancel found, or `None` if `future` names no read in
  /// flight on this set. No event follows for it, and the host no longer
  /// uses the read's memory.
  pub fn cancel_future(&mut self, future: ReadFuture) -> Option<Cancel> {
    let cancel = self.futures.get_mut(future.0 as usize)?.take()?;
    // As for calls: leave the set before cancelling synchronously.
    RawSet::leave(future.0);
    // SAFETY: the handle is a future's readable end with a read in flight.
    let code = unsafe { cancel(future.0) };
    let Some(found) = Cancel::decode_future(code) else {
      unreachable!("a synchronous future cancel returned BLOCKED")
    };
    self.in_flight -= 1;
    Some(found)
  }

  /// Cancels the read in flight on `stream`.
  ///
  /// Returns how the read ended, which is as [`StreamResult::Cancelled`]
  /// unless it finished first, or `None` if no read was in flight. No event
  /// follows for it, and the host no longer uses the read's memory.
  pub fn cancel_read(&mut self, stream: ReadStream) -> Option<StreamResult> {
    self.cancel_stream(stream.0, End::Readable)
  }

  /// Cancels the write in flight on `stream`, like
  /// [`cancel_read`](Self::cancel_read).
  pub fn cancel_write(&mut self, stream: WriteStream) -> Option<StreamResult> {
    self.cancel_stream(stream.0, End::Writable)
  }

  /// Cancels the accept in flight on `listener`, like
  /// [`cancel_read`](Self::cancel_read).
  pub fn cancel_accept(&mut self, listener: Listener) -> Option<StreamResult> {
    self.cancel_stream(listener.0, End::Accepting)
  }

  /// Cancels the read in flight on a directory stream, like
  /// [`cancel_read`](Self::cancel_read).
  pub fn cancel_read_dir(&mut self, stream: DirStream) -> Option<StreamResult> {
    self.cancel_stream(stream.0, End::Listing)
  }

  /// Closes the readable end, cancelling its read first if one is in flight.
  ///
  /// Returns how that read ended, if there was one. The writer on the other
  /// end sees the stream dropped.
  pub fn close_reader(
    &mut self,
    stream: ReadStream,
  ) -> Result<Option<StreamResult>, Error> {
    self.close(stream.0, End::Readable)
  }

  /// Closes the writable end, cancelling its write first if one is in
  /// flight.
  ///
  /// Returns how that write ended, if there was one. The reader on the
  /// other end sees the stream dropped once it has taken what was written.
  pub fn close_writer(
    &mut self,
    stream: WriteStream,
  ) -> Result<Option<StreamResult>, Error> {
    self.close(stream.0, End::Writable)
  }

  /// Stops listening, cancelling the accept in flight if there is one.
  ///
  /// Returns how that accept ended, if there was one.
  pub fn close_listener(
    &mut self,
    listener: Listener,
  ) -> Result<Option<StreamResult>, Error> {
    self.close(listener.0, End::Accepting)
  }

  /// Stops listing, cancelling the read in flight if there is one.
  ///
  /// Returns how that read ended, if there was one.
  pub fn close_dir_reader(
    &mut self,
    stream: DirStream,
  ) -> Result<Option<StreamResult>, Error> {
    self.close(stream.0, End::Listing)
  }

  /// Operations in flight: each ends in an [`Event`] or by its cancel.
  pub fn in_flight(&self) -> usize {
    self.in_flight
  }

  /// Returns the next event, blocking until one arrives.
  ///
  /// Returns `None` when nothing is in flight.
  pub fn wait(&mut self) -> Option<Event> {
    // Every pass handles one host event. One that ends nothing (a call
    // that only started) leaves its operation in flight, so the next pass
    // still has something to wait for.
    loop {
      if self.in_flight == 0 {
        return None;
      }
      let raw = self.set.wait();
      if let Some(event) = self.finish(raw) {
        return Some(event);
      }
    }
  }

  /// Returns the next event if one is ready, without blocking.
  pub fn poll(&mut self) -> Option<Event> {
    loop {
      if self.in_flight == 0 {
        return None;
      }
      let raw = self.set.poll()?;
      if let Some(event) = self.finish(raw) {
        return Some(event);
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
    let slot = Slot { end, blocked: false, dropped: false, cancel, drop };
    set_slot(&mut self.streams, handle, Some(slot));
  }

  /// The slot for `handle`, if the set owns it as an `end`.
  fn slot_mut(&mut self, handle: u32, end: End) -> Result<&mut Slot, Error> {
    self
      .streams
      .get_mut(handle as usize)
      .and_then(Option::as_mut)
      .filter(|slot| slot.end == end)
      .ok_or(Error::UnknownStream)
  }

  /// Checks that an operation may start on `handle`.
  ///
  /// Returns `false` when the other end was dropped: the operation must not
  /// reach the host, and is answered with `Dropped(0)`.
  fn begin(&mut self, handle: u32, end: End) -> Result<bool, Error> {
    let slot = self.slot_mut(handle, end)?;
    if slot.blocked {
      return Err(Error::Busy);
    }
    Ok(!slot.dropped)
  }

  /// Records what the host answered when a stream operation started.
  fn started(&mut self, handle: u32, code: u32) -> Started {
    let Some(Some(slot)) = self.streams.get_mut(handle as usize) else {
      unreachable!("an operation started on a stream with no slot")
    };
    match StreamResult::decode(code) {
      Some(result) => {
        slot.dropped |= matches!(result, StreamResult::Dropped(_));
        Started::Done(result)
      }
      None => {
        slot.blocked = true;
        self.set.join(handle);
        self.in_flight += 1;
        Started::Blocked
      }
    }
  }

  /// Cancels the operation in flight on a stream end of this kind.
  fn cancel_stream(&mut self, handle: u32, end: End) -> Option<StreamResult> {
    let slot = self.slot_mut(handle, end).ok()?;
    if !std::mem::take(&mut slot.blocked) {
      return None;
    }
    // As for calls: leave the set before cancelling synchronously.
    RawSet::leave(handle);
    // SAFETY: the handle is an open end of this kind with an operation in
    // flight.
    let code = unsafe { (slot.cancel)(handle) };
    let Some(result) = StreamResult::decode(code) else {
      unreachable!("a synchronous stream cancel returned BLOCKED")
    };
    slot.dropped |= matches!(result, StreamResult::Dropped(_));
    self.in_flight -= 1;
    Some(result)
  }

  fn close(
    &mut self,
    handle: u32,
    end: End,
  ) -> Result<Option<StreamResult>, Error> {
    let drop = self.slot_mut(handle, end)?.drop;
    let cancelled = self.cancel_stream(handle, end);
    self.streams[handle as usize] = None;
    // SAFETY: the handle is an open end of this kind with nothing in flight,
    // and its slot is gone, so it is dropped once.
    unsafe { drop(handle) };
    Ok(cancelled)
  }

  /// Handles one host event. Returns the event for the operation it ends,
  /// or `None` when the operation is still in flight.
  fn finish(&mut self, raw: RawEvent) -> Option<Event> {
    match raw.kind {
      sys::EVENT_SUBTASK => self.finish_call(raw),
      sys::EVENT_FUTURE_READ => Some(self.finish_future(raw)),
      _ => Some(self.finish_stream(raw)),
    }
  }

  /// Handles a state change of a call.
  fn finish_call(&mut self, raw: RawEvent) -> Option<Event> {
    let Some(live) = self.calls.get_mut(raw.waitable as usize) else {
      unreachable!("event for a call this set did not start: {raw:?}")
    };
    match raw.code {
      sys::STATUS_STARTING | sys::STATUS_STARTED => None,
      sys::STATUS_RETURNED => {
        if !std::mem::take(live) {
          unreachable!("event for a call that is not in flight: {raw:?}")
        }
        RawSet::leave(raw.waitable);
        sys::subtask_drop(raw.waitable);
        self.in_flight -= 1;
        Some(Event::Returned(Subtask(raw.waitable)))
      }
      other => unreachable!("a call moved to unexpected state {other:#x}"),
    }
  }

  /// Turns a host future event into the end of its read.
  fn finish_future(&mut self, raw: RawEvent) -> Event {
    let read =
      self.futures.get_mut(raw.waitable as usize).and_then(Option::take);
    if read.is_none() {
      unreachable!("event for a future with no read in flight: {raw:?}")
    }
    if Cancel::decode_future(raw.code) != Some(Cancel::Completed) {
      unreachable!("a future read ended without its value: {raw:?}")
    }
    RawSet::leave(raw.waitable);
    self.in_flight -= 1;
    Event::FutureRead(ReadFuture(raw.waitable))
  }

  /// Turns a host stream event into the end of the operation on it.
  fn finish_stream(&mut self, raw: RawEvent) -> Event {
    let Some(Some(slot)) = self.streams.get_mut(raw.waitable as usize) else {
      unreachable!("event for a stream this set does not own: {raw:?}")
    };
    if raw.kind != slot.end.event() {
      unreachable!("event does not match its stream end: {raw:?}")
    }
    if !std::mem::take(&mut slot.blocked) {
      unreachable!("event for a stream with nothing in flight: {raw:?}")
    }
    let Some(result) = StreamResult::decode(raw.code) else {
      unreachable!("a stream event carried BLOCKED")
    };
    slot.dropped |= matches!(result, StreamResult::Dropped(_));
    let end = slot.end;
    RawSet::leave(raw.waitable);
    self.in_flight -= 1;
    match end {
      End::Readable => Event::Read(ReadStream(raw.waitable), result),
      End::Writable => Event::Write(WriteStream(raw.waitable), result),
      End::Accepting => Event::Accept(Listener(raw.waitable), result),
      End::Listing => Event::ReadDir(DirStream(raw.waitable), result),
    }
  }
}

/// Stores `value` at `handle`, growing the table to reach it.
fn set_slot<T: Clone + Default>(table: &mut Vec<T>, handle: u32, value: T) {
  let index = handle as usize;
  if table.len() <= index {
    table.resize(index + 1, T::default());
  }
  table[index] = value;
}

impl Default for WaitableSet {
  fn default() -> Self {
    Self::new()
  }
}

impl Drop for WaitableSet {
  fn drop(&mut self) {
    // Stop every operation so the host stops using caller memory, then drop
    // the stream ends. The set drops after this, once it is empty. Each
    // index came from a `u32` handle.
    for index in 0..self.calls.len() {
      self.cancel(Subtask(index as u32));
    }
    for index in 0..self.futures.len() {
      self.cancel_future(ReadFuture(index as u32));
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
    assert_eq!(StreamResult::decode(5 << 4), Some(StreamResult::Completed(5)));
    assert_eq!(
      StreamResult::decode((3 << 4) | 1),
      Some(StreamResult::Dropped(3))
    );
    assert_eq!(StreamResult::decode(2), Some(StreamResult::Cancelled(0)));
  }

  #[test]
  fn blocked_is_not_a_result() {
    assert_eq!(StreamResult::decode(sys::BLOCKED), None);
    assert_eq!(Cancel::decode_future(sys::BLOCKED), None);
  }

  #[test]
  fn a_future_read_delivers_or_is_cancelled() {
    assert_eq!(Cancel::decode_future(0), Some(Cancel::Completed));
    assert_eq!(Cancel::decode_future(2), Some(Cancel::Cancelled));
  }

  #[test]
  fn count_reports_items_for_every_result() {
    assert_eq!(StreamResult::Completed(7).count(), 7);
    assert_eq!(StreamResult::Dropped(2).count(), 2);
    assert_eq!(StreamResult::Cancelled(1).count(), 1);
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
    set_slot(&mut table, 3, Some(9));
    assert_eq!(table, [None, None, None, Some(9)]);
    set_slot(&mut table, 1, Some(4));
    assert_eq!(table, [None, Some(4), None, Some(9)]);
  }
}
