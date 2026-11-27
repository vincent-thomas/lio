//! Exercises a [`lio_wasi::WaitableSet`] against an in-component stream,
//! where both ends are in the set and every result is known exactly.
//!
//! ```text
//! cargo build -p lio-wasi --example pipe --target wasm32-wasip3
//! wasmtime run -S p3 target/wasm32-wasip3/debug/examples/pipe.wasm
//! ```
//!
//! It also copies standard input to standard output, to cover a host stream
//! in each direction.

use lio_wasi::{
  Error, Event, ReadStream, Started, StreamResult, WaitableSet, WriteStream,
};

/// Waits until nothing is in flight. Returns how the read on `reader` and
/// the write on `writer` ended, for each that an event reported.
fn settle(
  set: &mut WaitableSet,
  reader: ReadStream,
  writer: WriteStream,
) -> (Option<StreamResult>, Option<StreamResult>) {
  let (mut read, mut wrote) = (None, None);
  while let Some(event) = set.wait() {
    match event {
      Event::Read(stream, result) if stream == reader => read = Some(result),
      Event::Write(stream, result) if stream == writer => wrote = Some(result),
      other => unreachable!("an event for nothing started here: {other:?}"),
    }
  }
  (read, wrote)
}

/// The result of an operation that either ended as it started or was
/// reported by an event.
fn ended(started: Started, event: Option<StreamResult>) -> StreamResult {
  match (started, event) {
    (Started::Done(result), None) | (Started::Blocked, Some(result)) => result,
    (started, event) => {
      panic!("an operation that started as {started:?} had event {event:?}")
    }
  }
}

/// A read blocks until the write arrives; both then end.
fn blocked_read_meets_write() -> Result<(), Error> {
  let (writer, reader) = wasip3::wit_stream::new::<u8>();
  let mut set = WaitableSet::new();
  let reader = set.add_reader(reader);
  let writer = set.add_writer(writer);

  let mut buf = [0u8; 8];
  // SAFETY: `buf` and the literal outlive both operations, which have ended
  // once `settle` returns.
  let (read, write) = unsafe {
    let read = set.read(reader, buf.as_mut_ptr(), buf.len())?;
    assert_eq!(read, Started::Blocked, "the read has nothing to take yet");
    assert!(set.poll().is_none());
    (read, set.write(writer, b"hello".as_ptr(), 5)?)
  };
  let (read_event, write_event) = settle(&mut set, reader, writer);
  assert_eq!(ended(read, read_event), StreamResult::Completed(5));
  assert_eq!(ended(write, write_event), StreamResult::Completed(5));
  assert_eq!(&buf[..5], b"hello");
  assert_eq!(set.in_flight(), 0);
  println!("blocked read meets write: ok");
  Ok(())
}

/// Cancelling a blocked read answers `Cancelled(0)` with no event, and the
/// stream is usable afterwards.
fn cancel_blocked_read() -> Result<(), Error> {
  let (writer, reader) = wasip3::wit_stream::new::<u8>();
  let mut set = WaitableSet::new();
  let reader = set.add_reader(reader);
  let writer = set.add_writer(writer);

  let mut buf = [0u8; 8];
  // SAFETY: `buf` outlives the read, which the cancel below ends.
  let read = unsafe { set.read(reader, buf.as_mut_ptr(), buf.len())? };
  assert_eq!(read, Started::Blocked);
  assert_eq!(
    unsafe { set.read(reader, buf.as_mut_ptr(), buf.len()) },
    Err(Error::Busy)
  );
  assert_eq!(set.cancel_read(reader), Some(StreamResult::Cancelled(0)));
  assert_eq!(set.cancel_read(reader), None, "the read is no longer in flight");
  assert!(set.wait().is_none());

  // SAFETY: as above, until `settle` returns.
  let (read, write) = unsafe {
    (
      set.read(reader, buf.as_mut_ptr(), buf.len())?,
      set.write(writer, b"again".as_ptr(), 5)?,
    )
  };
  let (read_event, write_event) = settle(&mut set, reader, writer);
  let total =
    ended(read, read_event).count() + ended(write, write_event).count();
  assert_eq!(total, 10);
  assert_eq!(&buf[..5], b"again");
  println!("cancel blocked read: ok");
  Ok(())
}

/// Closing the writer ends the reader's stream, and later reads are answered
/// without reaching the host.
fn close_writer_ends_reader() -> Result<(), Error> {
  let (writer, reader) = wasip3::wit_stream::new::<u8>();
  let mut set = WaitableSet::new();
  let reader = set.add_reader(reader);
  let writer = set.add_writer(writer);

  let mut buf = [0u8; 8];
  // SAFETY: `buf` outlives the read, whose end the wait below reports.
  let read = unsafe { set.read(reader, buf.as_mut_ptr(), buf.len())? };
  assert_eq!(read, Started::Blocked);
  assert_eq!(set.close_writer(writer)?, None, "no write was in flight");
  assert_eq!(set.wait(), Some(Event::Read(reader, StreamResult::Dropped(0))));
  assert_eq!(
    unsafe { set.write(writer, b"x".as_ptr(), 1) },
    Err(Error::UnknownStream)
  );
  // SAFETY: as above; this read ends as it starts.
  let read = unsafe { set.read(reader, buf.as_mut_ptr(), buf.len())? };
  assert_eq!(read, Started::Done(StreamResult::Dropped(0)));
  println!("close writer ends reader: ok");
  Ok(())
}

/// Closing a stream end with an operation in flight cancels it and says how
/// it ended.
fn close_cancels_what_is_in_flight() -> Result<(), Error> {
  let (_writer, reader) = wasip3::wit_stream::new::<u8>();
  let mut set = WaitableSet::new();
  let reader = set.add_reader(reader);

  let mut buf = [0u8; 8];
  // SAFETY: `buf` outlives the read, which the close below ends.
  let read = unsafe { set.read(reader, buf.as_mut_ptr(), buf.len())? };
  assert_eq!(read, Started::Blocked);
  assert_eq!(set.close_reader(reader)?, Some(StreamResult::Cancelled(0)));
  assert_eq!(set.in_flight(), 0);
  assert_eq!(set.close_reader(reader), Err(Error::UnknownStream));
  println!("close cancels what is in flight: ok");
  Ok(())
}

/// Dropping the set with operations in flight cancels them and traps
/// nothing.
fn drop_with_operations_in_flight() -> Result<(), Error> {
  let (writer, reader) = wasip3::wit_stream::new::<u8>();
  let (idle_writer, idle_reader) = wasip3::wit_stream::new::<u8>();
  let mut set = WaitableSet::new();
  let reader = set.add_reader(reader);
  set.add_writer(writer);
  set.add_reader(idle_reader);
  let idle_writer = set.add_writer(idle_writer);

  let mut buf = [0u8; 8];
  // SAFETY: the set is dropped, cancelling both, before `buf` goes away.
  unsafe {
    let read = set.read(reader, buf.as_mut_ptr(), buf.len())?;
    let write = set.write(idle_writer, b"unread".as_ptr(), 6)?;
    assert_eq!((read, write), (Started::Blocked, Started::Blocked));
  }
  assert_eq!(set.in_flight(), 2);
  drop(set);
  println!("drop with operations in flight: ok");
  Ok(())
}

/// Copies standard input to standard output through host streams.
fn copy_stdin_to_stdout() -> Result<(), Error> {
  let (stdin, _stdin_result) = wasip3::cli::stdin::read_via_stream();
  let (out, out_reader) = wasip3::wit_stream::new::<u8>();
  let _stdout_result = wasip3::cli::stdout::write_via_stream(out_reader);
  let mut set = WaitableSet::new();
  let stdin = set.add_reader(stdin);
  let out = set.add_writer(out);

  let mut buf = [0u8; 4096];
  let mut copied = 0;
  loop {
    // SAFETY: `buf` outlives each operation; it is reused only after the
    // operation using it has ended, which `settle` waits for.
    let started = unsafe { set.read(stdin, buf.as_mut_ptr(), buf.len())? };
    let read = ended(started, settle(&mut set, stdin, out).0);
    let mut sent = 0;
    while sent < read.count() {
      let rest = &buf[sent..read.count()];
      // SAFETY: as above.
      let started = unsafe { set.write(out, rest.as_ptr(), rest.len())? };
      let wrote = ended(started, settle(&mut set, stdin, out).1);
      assert!(matches!(wrote, StreamResult::Completed(_)), "stdout closed");
      sent += wrote.count();
    }
    copied += sent;
    if let StreamResult::Dropped(_) = read {
      break;
    }
  }
  set.close_writer(out)?;
  eprintln!("copied {copied} bytes");
  Ok(())
}

fn main() -> Result<(), Error> {
  blocked_read_meets_write()?;
  cancel_blocked_read()?;
  close_writer_ends_reader()?;
  close_cancels_what_is_in_flight()?;
  drop_with_operations_in_flight()?;
  copy_stdin_to_stdout()
}
