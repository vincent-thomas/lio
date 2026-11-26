//! Exercises a [`lio_wasi::Queue`] against an in-component stream, where
//! both ends are in the queue and every result is known exactly.
//!
//! ```text
//! cargo build -p lio-wasi --example pipe --target wasm32-wasip3
//! wasmtime run -S p3 target/wasm32-wasip3/debug/examples/pipe.wasm
//! ```
//!
//! It also copies standard input to standard output, to cover a host stream
//! in each direction.

use lio_wasi::{Completion, Error, Outcome, Queue};

const READ: u64 = 1;
const WRITE: u64 = 2;

fn next(queue: &mut Queue) -> Completion {
  let Some(done) = queue.wait() else {
    unreachable!("an operation is outstanding")
  };
  done
}

/// A read blocks until the write arrives; both then complete.
fn blocked_read_meets_write() -> Result<(), Error> {
  let (writer, reader) = wasip3::wit_stream::new::<u8>();
  let mut queue = Queue::new();
  let reader = queue.add_reader(reader);
  let writer = queue.add_writer(writer);

  let mut buf = [0u8; 8];
  // SAFETY: `buf` and the literal outlive both completions taken below.
  unsafe {
    queue.push_read(reader, buf.as_mut_ptr(), buf.len(), READ)?;
    assert!(queue.poll().is_none(), "the read has nothing to take yet");
    queue.push_write(writer, b"hello".as_ptr(), 5, WRITE)?;
  }
  let mut seen = [next(&mut queue), next(&mut queue)];
  seen.sort_by_key(Completion::user_data);
  assert_eq!(seen[0].user_data(), READ);
  assert_eq!(seen[0].outcome(), Outcome::Transferred(5));
  assert_eq!(seen[1].user_data(), WRITE);
  assert_eq!(seen[1].outcome(), Outcome::Transferred(5));
  assert_eq!(&buf[..5], b"hello");
  assert_eq!(queue.outstanding(), 0);
  println!("blocked read meets write: ok");
  Ok(())
}

/// Cancelling a blocked read yields exactly one `Cancelled(0)` completion,
/// and the stream is usable afterwards.
fn cancel_blocked_read() -> Result<(), Error> {
  let (writer, reader) = wasip3::wit_stream::new::<u8>();
  let mut queue = Queue::new();
  let reader = queue.add_reader(reader);
  let writer = queue.add_writer(writer);

  let mut buf = [0u8; 8];
  // SAFETY: `buf` outlives the completions taken below.
  unsafe { queue.push_read(reader, buf.as_mut_ptr(), buf.len(), READ)? };
  assert_eq!(
    unsafe { queue.push_read(reader, buf.as_mut_ptr(), buf.len(), 9) },
    Err(Error::Busy)
  );
  assert!(queue.cancel(READ));
  assert!(!queue.cancel(READ), "a cancelled read is no longer in flight");
  let done = next(&mut queue);
  assert_eq!(done.user_data(), READ);
  assert_eq!(done.outcome(), Outcome::Cancelled(0));
  assert!(queue.wait().is_none());

  // SAFETY: as above.
  unsafe {
    queue.push_read(reader, buf.as_mut_ptr(), buf.len(), READ)?;
    queue.push_write(writer, b"again".as_ptr(), 5, WRITE)?;
  }
  let total =
    next(&mut queue).outcome().count() + next(&mut queue).outcome().count();
  assert_eq!(total, 10);
  assert_eq!(&buf[..5], b"again");
  println!("cancel blocked read: ok");
  Ok(())
}

/// Closing the writer ends the reader's stream, and later reads are answered
/// without reaching the host.
fn close_writer_ends_reader() -> Result<(), Error> {
  let (writer, reader) = wasip3::wit_stream::new::<u8>();
  let mut queue = Queue::new();
  let reader = queue.add_reader(reader);
  let writer = queue.add_writer(writer);

  let mut buf = [0u8; 8];
  // SAFETY: `buf` outlives the completions taken below.
  unsafe { queue.push_read(reader, buf.as_mut_ptr(), buf.len(), READ)? };
  queue.close_writer(writer)?;
  assert_eq!(next(&mut queue).outcome(), Outcome::Closed(0));
  assert_eq!(
    unsafe { queue.push_write(writer, b"x".as_ptr(), 1, WRITE) },
    Err(Error::UnknownStream)
  );
  // SAFETY: as above.
  unsafe { queue.push_read(reader, buf.as_mut_ptr(), buf.len(), READ)? };
  assert_eq!(next(&mut queue).outcome(), Outcome::Closed(0));
  println!("close writer ends reader: ok");
  Ok(())
}

/// Dropping the queue with operations in flight cancels them and traps
/// nothing.
fn drop_with_operations_in_flight() -> Result<(), Error> {
  let (writer, reader) = wasip3::wit_stream::new::<u8>();
  let (idle_writer, idle_reader) = wasip3::wit_stream::new::<u8>();
  let mut queue = Queue::new();
  let reader = queue.add_reader(reader);
  queue.add_writer(writer);
  queue.add_reader(idle_reader);
  let idle_writer = queue.add_writer(idle_writer);

  let mut buf = [0u8; 8];
  // SAFETY: the queue is dropped, cancelling both, before `buf` goes away.
  unsafe {
    queue.push_read(reader, buf.as_mut_ptr(), buf.len(), READ)?;
    queue.push_write(idle_writer, b"unread".as_ptr(), 6, WRITE)?;
  }
  assert_eq!(queue.outstanding(), 2);
  drop(queue);
  println!("drop with operations in flight: ok");
  Ok(())
}

/// Copies standard input to standard output through host streams.
fn copy_stdin_to_stdout() -> Result<(), Error> {
  let (stdin, _stdin_result) = wasip3::cli::stdin::read_via_stream();
  let (out, out_reader) = wasip3::wit_stream::new::<u8>();
  let _stdout_result = wasip3::cli::stdout::write_via_stream(out_reader);
  let mut queue = Queue::new();
  let stdin = queue.add_reader(stdin);
  let out = queue.add_writer(out);

  let mut buf = [0u8; 4096];
  let mut copied = 0;
  loop {
    // SAFETY: `buf` outlives each completion; it is reused only after the
    // operation using it has completed.
    unsafe { queue.push_read(stdin, buf.as_mut_ptr(), buf.len(), READ)? };
    let read = next(&mut queue).outcome();
    let mut sent = 0;
    while sent < read.count() {
      let rest = &buf[sent..read.count()];
      // SAFETY: as above.
      unsafe { queue.push_write(out, rest.as_ptr(), rest.len(), WRITE)? };
      let wrote = next(&mut queue).outcome();
      assert!(matches!(wrote, Outcome::Transferred(_)), "stdout closed");
      sent += wrote.count();
    }
    copied += sent;
    if let Outcome::Closed(_) = read {
      break;
    }
  }
  queue.close_writer(out)?;
  eprintln!("copied {copied} bytes");
  Ok(())
}

fn main() -> Result<(), Error> {
  blocked_read_meets_write()?;
  cancel_blocked_read()?;
  close_writer_ends_reader()?;
  drop_with_operations_in_flight()?;
  copy_stdin_to_stdout()
}
