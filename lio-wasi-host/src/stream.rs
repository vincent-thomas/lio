//! Byte streams between the guest and lio.
//!
//! A guest's read names a range of its own memory. On a [`crate::Store`]
//! that range is the lio operation's buffer ([`crate::lend`]), so the
//! kernel fills it directly.
//!
//! A guest's write is the same the other way: the kernel reads the bytes
//! from the guest's range.
//!
//! A stream the guest got from the host and handed to another host function
//! has no guest range at either end. Its reads and writes keep a host
//! buffer and one copy. [`Origin`] tells the two apart for a write.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use lio::api;
use lio::api::ops::{Read, Recv, Send as SendOp, Stat, Write};
use lio::api::resource::Resource;
use wasmtime::StoreContextMut;
use wasmtime::component::{
  Destination, ReadBuffer, Source, StreamConsumer, StreamProducer,
  StreamResult, VecBuffer, WriteBuffer,
};

use crate::io::{Guess, Op, op};
use crate::lend::{LendingRef, Lent, StreamLease, count};

/// The most one operation reads or writes.
const MAX_IO: usize = 1 << 20;

pub(crate) use crate::status::Status;
use crate::status::{Ends, Sender, channel};

/// Where a [`ByteReader`] reads from.
pub(crate) enum ReadFrom {
  /// A file, from this offset on.
  File(Resource, u64),
  /// A pipe or terminal: wherever its cursor is.
  Cursor(Resource),
  /// A connected socket.
  Socket(Resource),
}

enum Reading {
  /// Into the guest's own range.
  Lent,
  Read(Op<Read<Vec<u8>>>),
  Recv(Op<Recv<Vec<u8>>>),
}

/// The host end of a `stream<u8>` the guest reads.
pub(crate) struct ByteReader<E: Ends> {
  from: ReadFrom,
  /// Bytes read and not yet given to the guest are `buf[given..]`.
  buf: Vec<u8>,
  given: usize,
  /// The read in flight. It outlives a cancelled guest read, so nothing it
  /// reads is lost: the next guest read gets it.
  reading: Option<Reading>,
  /// Tells the guest how the stream ended. Gone once it has.
  status: Option<Sender<E>>,
  error: fn(&io::Error) -> E,
  /// Runs when the guest drops its end.
  on_drop: fn(&Resource),
  /// Present on a [`crate::Store`]: guest ranges may be lent to lio.
  lending: Option<LendingRef>,
  /// The lease its reads into guest memory go through, made by the first.
  lease: StreamLease,
  /// Whether reads here have been finishing as soon as they are submitted.
  guess: Guess,
  /// The read in flight was submitted as it started.
  tried: bool,
}

impl<E: Ends> ByteReader<E> {
  pub(crate) fn new(
    from: ReadFrom,
    error: fn(&io::Error) -> E,
    on_drop: fn(&Resource),
    lending: Option<LendingRef>,
  ) -> (Self, Status<E>) {
    let (status, ended) = channel();
    let guess = match &from {
      // A file's data is usually there. A peer's has to arrive.
      ReadFrom::File(..) => Guess::default(),
      ReadFrom::Cursor(_) | ReadFrom::Socket(_) => Guess::waits(),
    };
    let reader = Self {
      lending,
      lease: StreamLease::default(),
      guess,
      tried: false,
      from,
      buf: Vec::new(),
      given: 0,
      reading: None,
      status: Some(status),
      error,
      on_drop,
    };
    (reader, ended)
  }

  fn end(&mut self, how: Result<(), E>) {
    if let Some(status) = self.status.take() {
      status.send(how);
    }
  }

  fn resource(&self) -> &Resource {
    match &self.from {
      ReadFrom::File(resource, _)
      | ReadFrom::Cursor(resource)
      | ReadFrom::Socket(resource) => resource,
    }
  }
}

impl<E: Ends> Drop for ByteReader<E> {
  fn drop(&mut self) {
    if let (Some(Reading::Lent), Some(lease)) =
      (&self.reading, self.lease.get())
    {
      lease.cancel();
    }
    self.end(Ok(()));
    (self.on_drop)(self.resource());
  }
}

impl<D, E: Ends + Send> StreamProducer<D> for ByteReader<E> {
  type Item = u8;
  type Buffer = VecBuffer<u8>;

  fn poll_produce<'a>(
    self: Pin<&mut Self>,
    cx: &mut Context<'_>,
    mut store: StoreContextMut<'a, D>,
    dst: Destination<'a, Self::Item, Self::Buffer>,
    finish: bool,
  ) -> Poll<wasmtime::Result<StreamResult>> {
    let me = self.get_mut();
    let wanted = dst.remaining(&mut store)?;
    // A read of nothing asks whether the stream is readable. Saying yes is
    // allowed: the read that follows waits.
    if wanted == Some(0) {
      return Poll::Ready(Ok(StreamResult::Completed));
    }

    let size = wanted.unwrap_or(MAX_IO).min(MAX_IO);
    let mut direct = dst.as_direct(store, size);

    if me.given == me.buf.len() {
      if me.status.is_none() {
        return Poll::Ready(Ok(StreamResult::Dropped));
      }
      let starting = me.reading.is_none();
      if starting {
        let likely = me.guess.likely();
        me.tried = false;
        me.reading = Some(match (&me.lending, wanted) {
          (Some(lending), Some(_)) => {
            let room = direct.remaining()?;
            let len = room.len().min(MAX_IO);
            let room = &mut room[..len];
            // SAFETY: `room` is the buffer of the guest read this poll is
            // for, on a `crate::Store`. Below, the read is reported over
            // only once the lease is `Ready`.
            let done = unsafe {
              match &me.from {
                ReadFrom::File(file, offset) => {
                  lending.read(&mut me.lease, room, |buf| {
                    api::read_at(file, buf, *offset)
                  })
                }
                ReadFrom::Cursor(file) => {
                  lending.read(&mut me.lease, room, |buf| api::read(file, buf))
                }
                ReadFrom::Socket(socket) => {
                  lending.read(&mut me.lease, room, |buf| {
                    api::recv(socket, buf, None)
                  })
                }
              }
            };
            // Most are done already. One that is not is given the chance
            // to finish within this call, and remembered with the store.
            if !done {
              me.tried = crate::exec::drive(likely);
              me.lease.track();
            }
            Reading::Lent
          }
          _ => {
            count(|stats| stats.reads_copied += 1);
            let mut buf = std::mem::take(&mut me.buf);
            buf.clear();
            me.given = 0;
            if buf.capacity() < size {
              buf = Vec::with_capacity(size);
            }
            match &me.from {
              ReadFrom::File(file, offset) => Reading::Read(
                op(api::read_at(file, buf, *offset)).likely(likely),
              ),
              ReadFrom::Cursor(file) => {
                Reading::Read(op(api::read(file, buf)).likely(likely))
              }
              ReadFrom::Socket(socket) => {
                Reading::Recv(op(api::recv(socket, buf, None)).likely(likely))
              }
            }
          }
        });
      }
      let polled = match &mut me.reading {
        Some(Reading::Lent) => {
          let Some(lease) = me.lease.get() else {
            unreachable!("a read was lent with no lease");
          };
          // The guest's range is the kernel's until the lease says
          // otherwise, cancelled or not.
          let lent = lease.poll(cx, finish);
          if starting && me.tried {
            me.guess.record(lent.is_ready());
          }
          let lent = match lent {
            Poll::Ready(lent) => lent,
            Poll::Pending => return Poll::Pending,
          };
          me.reading = None;
          return Poll::Ready(Ok(match lent {
            Lent::Done(Ok(0)) => {
              me.end(Ok(()));
              StreamResult::Dropped
            }
            Lent::Done(Ok(read)) => {
              direct.mark_written(read);
              if let ReadFrom::File(_, offset) = &mut me.from {
                *offset += read as u64;
              }
              StreamResult::Completed
            }
            Lent::Done(Err(error)) => {
              let error = (me.error)(&error);
              me.end(Err(error));
              StreamResult::Dropped
            }
            Lent::Cancelled => StreamResult::Cancelled,
          }));
        }
        Some(Reading::Read(read)) => Pin::new(read).poll(cx),
        Some(Reading::Recv(recv)) => Pin::new(recv).poll(cx),
        None => unreachable!("a read was just started"),
      };
      let (result, buf) = match polled {
        Poll::Ready(done) => done,
        Poll::Pending if finish => {
          return Poll::Ready(Ok(StreamResult::Cancelled));
        }
        Poll::Pending => return Poll::Pending,
      };
      me.reading = None;
      me.buf = buf;
      me.given = 0;
      match result {
        Ok(0) => {
          me.buf.clear();
          me.end(Ok(()));
          return Poll::Ready(Ok(StreamResult::Dropped));
        }
        Ok(read) => {
          if let ReadFrom::File(_, offset) = &mut me.from {
            *offset += read as u64;
          }
        }
        Err(error) => {
          me.buf.clear();
          let error = (me.error)(&error);
          me.end(Err(error));
          return Poll::Ready(Ok(StreamResult::Dropped));
        }
      }
    }

    let ready = &me.buf[me.given..];
    let room = direct.remaining()?;
    let count = room.len().min(ready.len());
    room[..count].copy_from_slice(&ready[..count]);
    direct.mark_written(count);
    me.given += count;
    Poll::Ready(Ok(StreamResult::Completed))
  }
}

/// Where a [`ByteWriter`] writes to.
pub(crate) enum WriteTo {
  /// A file, from this offset on.
  File(Resource, u64),
  /// The end of a file, wherever that is when each write starts.
  Append(Resource),
  /// A pipe or terminal: wherever its cursor is.
  Cursor(Resource),
  /// A connected socket.
  Socket(Resource),
}

enum Writing {
  /// Finding the end of the file an append goes to.
  Size(Op<Stat>),
  /// From the guest's own range.
  Lent,
  Write(Op<Write<Vec<u8>>>),
  Send(Op<SendOp<Vec<u8>>>),
}

/// Asks wasmtime whose bytes a write holds, without taking any.
///
/// wasmtime hands a consumer the writer's bytes without saying whether they
/// are in guest memory or in a host buffer, and only guest memory stays in
/// place for a whole operation. It does treat the two differently when
/// asked to move items into a [`ReadBuffer`]: a guest's are lifted out of
/// its memory and given to [`extend`](ReadBuffer::extend), and a host
/// buffer is passed to [`move_from`](ReadBuffer::move_from), which only a
/// host buffer can be. This buffer has room for one item and keeps none, so
/// asking consumes nothing.
#[derive(Default)]
struct Origin {
  lifted: bool,
  moved: bool,
}

impl ReadBuffer<u8> for Origin {
  fn extend<I: IntoIterator<Item = u8>>(&mut self, iter: I) {
    self.lifted |= iter.into_iter().next().is_some();
  }

  fn remaining_capacity(&self) -> usize {
    1
  }

  fn move_from(&mut self, _input: &mut dyn WriteBuffer<u8>, _count: usize) {
    self.moved = true;
  }
}

impl Origin {
  /// Whether the write in `src` is a guest's. Only a byte lifted from guest
  /// memory says so: anything else is treated as a host buffer, and copied.
  fn is_guest<D>(
    src: &mut Source<'_, u8>,
    store: &mut StoreContextMut<'_, D>,
  ) -> wasmtime::Result<bool> {
    let mut origin = Origin::default();
    src.read(store, &mut origin)?;
    Ok(origin.lifted && !origin.moved)
  }
}

/// The host end of a `stream<u8>` the guest writes.
pub(crate) struct ByteWriter<E: Ends> {
  to: WriteTo,
  /// The bytes of the write in flight, or spare capacity.
  buf: Vec<u8>,
  writing: Option<Writing>,
  status: Option<Sender<E>>,
  error: fn(&io::Error) -> E,
  /// Runs when the guest drops its end.
  on_drop: fn(&Resource),
  /// Present on a [`crate::Store`]: guest ranges may be lent to lio.
  lending: Option<LendingRef>,
  /// The lease its writes from guest memory go through, made by the first.
  lease: StreamLease,
  /// Whether the write being served is a guest's.
  guest: bool,
  /// Whether writes here have been finishing as soon as they are
  /// submitted.
  guess: Guess,
  /// The write in flight was submitted as it started.
  tried: bool,
}

impl<E: Ends> ByteWriter<E> {
  pub(crate) fn new(
    to: WriteTo,
    error: fn(&io::Error) -> E,
    on_drop: fn(&Resource),
    lending: Option<LendingRef>,
  ) -> (Self, Status<E>) {
    let (status, ended) = channel();
    let writer = Self {
      lending,
      lease: StreamLease::default(),
      guest: false,
      guess: Guess::default(),
      tried: false,
      to,
      buf: Vec::new(),
      writing: None,
      status: Some(status),
      error,
      on_drop,
    };
    (writer, ended)
  }

  fn end(&mut self, how: Result<(), E>) {
    if let Some(status) = self.status.take() {
      status.send(how);
    }
  }

  fn resource(&self) -> &Resource {
    match &self.to {
      WriteTo::File(resource, _)
      | WriteTo::Append(resource)
      | WriteTo::Cursor(resource)
      | WriteTo::Socket(resource) => resource,
    }
  }

  fn fail(
    &mut self,
    error: &io::Error,
  ) -> Poll<wasmtime::Result<StreamResult>> {
    self.writing = None;
    let error = (self.error)(error);
    self.end(Err(error));
    Poll::Ready(Ok(StreamResult::Dropped))
  }
}

impl<E: Ends> Drop for ByteWriter<E> {
  fn drop(&mut self) {
    if let (Some(Writing::Lent), Some(lease)) =
      (&self.writing, self.lease.get())
    {
      lease.cancel();
    }
    self.end(Ok(()));
    (self.on_drop)(self.resource());
  }
}

impl<E: Ends> ByteWriter<E> {
  /// Starts the write of `data`: from the guest's range if it is one, from
  /// a copy otherwise. An append has its file's size in `end`.
  fn start(&mut self, data: &[u8], end: Option<u64>) -> Writing {
    let data = &data[..data.len().min(MAX_IO)];
    let likely = self.guess.likely();
    self.tried = false;
    if let (true, Some(lending)) = (self.guest, &self.lending) {
      let stream = &mut self.lease;
      // SAFETY: `data` is the buffer of the guest write this poll is for
      // (`Origin` said so), on a `crate::Store`. The write is reported over
      // only once the lease is `Ready`.
      let done = unsafe {
        match (&self.to, end) {
          (WriteTo::File(file, offset), _) => {
            lending.write(stream, data, |buf| api::write_at(file, buf, *offset))
          }
          (WriteTo::Append(file), Some(end)) => {
            lending.write(stream, data, |buf| api::write_at(file, buf, end))
          }
          (WriteTo::Append(file), None) => {
            return Writing::Size(op(api::fstat(file)));
          }
          (WriteTo::Cursor(file), _) => {
            lending.write(stream, data, |buf| api::write(file, buf))
          }
          (WriteTo::Socket(socket), _) => {
            lending.write(stream, data, |buf| api::send(socket, buf, None))
          }
        }
      };
      // Most are done already. One that is not is given the chance to
      // finish within this call, and remembered with the store.
      if !done {
        self.tried = crate::exec::drive(likely);
        self.lease.track();
      }
      return Writing::Lent;
    }
    if let (WriteTo::Append(file), None) = (&self.to, end) {
      return Writing::Size(op(api::fstat(file)));
    }
    count(|stats| stats.writes_copied += 1);
    let mut buf = std::mem::take(&mut self.buf);
    buf.clear();
    buf.extend_from_slice(data);
    match (&self.to, end) {
      (WriteTo::File(file, offset), _) => {
        Writing::Write(op(api::write_at(file, buf, *offset)).likely(likely))
      }
      (WriteTo::Append(file), end) => Writing::Write(
        op(api::write_at(file, buf, end.unwrap_or(0))).likely(likely),
      ),
      (WriteTo::Cursor(file), _) => {
        Writing::Write(op(api::write(file, buf)).likely(likely))
      }
      (WriteTo::Socket(socket), _) => {
        Writing::Send(op(api::send(socket, buf, None)).likely(likely))
      }
    }
  }
}

impl<D, E: Ends + Send> StreamConsumer<D> for ByteWriter<E> {
  type Item = u8;

  fn poll_consume(
    self: Pin<&mut Self>,
    cx: &mut Context<'_>,
    mut store: StoreContextMut<D>,
    mut src: Source<Self::Item>,
    _finish: bool,
  ) -> Poll<wasmtime::Result<StreamResult>> {
    let me = self.get_mut();
    if me.status.is_none() {
      return Poll::Ready(Ok(StreamResult::Dropped));
    }
    if me.writing.is_none() {
      me.guest =
        me.lending.is_some() && Origin::is_guest(&mut src, &mut store)?;
    }
    let mut src = src.as_direct(store);

    if me.writing.is_none() {
      let data = src.remaining()?;
      // A write of nothing asks whether the stream is writable.
      if data.is_empty() {
        return Poll::Ready(Ok(StreamResult::Completed));
      }
      me.writing = Some(me.start(data, None));
      if let (true, Some(Writing::Lent), Some(lease)) =
        (me.tried, &me.writing, me.lease.get())
      {
        let done = lease.is_done();
        me.guess.record(done);
      }
    }

    // A write that has started is not cancelled: its bytes may already be
    // on their way. `finish` waits for it like any other poll.
    loop {
      let result = match &mut me.writing {
        Some(Writing::Size(stat)) => match Pin::new(stat).poll(cx) {
          Poll::Ready(Ok(stat)) => {
            me.writing = Some(me.start(src.remaining()?, Some(stat.size)));
            continue;
          }
          Poll::Ready(Err(error)) => return me.fail(&error),
          Poll::Pending => return Poll::Pending,
        },
        Some(Writing::Lent) => {
          let Some(lease) = me.lease.get() else {
            unreachable!("a write was lent with no lease");
          };
          match lease.poll(cx, false) {
            Poll::Ready(Lent::Done(result)) => result,
            // Only a store being dropped stops a write.
            Poll::Ready(Lent::Cancelled) => {
              Err(io::Error::from(io::ErrorKind::Interrupted))
            }
            Poll::Pending => return Poll::Pending,
          }
        }
        Some(Writing::Write(write)) => match Pin::new(write).poll(cx) {
          Poll::Ready((result, buf)) => {
            me.buf = buf;
            result
          }
          Poll::Pending => return Poll::Pending,
        },
        Some(Writing::Send(send)) => match Pin::new(send).poll(cx) {
          Poll::Ready((result, buf)) => {
            me.buf = buf;
            result
          }
          Poll::Pending => return Poll::Pending,
        },
        None => unreachable!("a write was just started"),
      };
      me.writing = None;
      return match result {
        Ok(written) => {
          src.mark_read(written);
          if let WriteTo::File(_, offset) = &mut me.to {
            *offset += written as u64;
          }
          Poll::Ready(Ok(StreamResult::Completed))
        }
        Err(error) => me.fail(&error),
      };
    }
  }
}
