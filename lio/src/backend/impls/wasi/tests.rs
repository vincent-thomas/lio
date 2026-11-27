//! The WASI backend against its real host.
//!
//! These drive [`Wasi`] through [`IoBackend`] the way the driver does, and
//! check each raw result. They need a preopened `/` to write under and
//! network access for loopback sockets; `mise run wasi` runs them under
//! wasmtime with both.
//!
//! The shared contract suite (`lio_test::test_io_backend!`) cannot run
//! here: it builds its fixtures from std descriptors, and a WASI resource is
//! not one. These cover the same queue and result rules, and the parts of
//! this backend the contract has no case for.

use std::net::{Ipv6Addr, SocketAddr};
use std::time::{Duration, Instant};

use super::*;
use crate::api::op::Completion as Done;
use crate::api::resource::Resource;
use crate::backend::InFlight;
use crate::backend::op::{
  BorrowedMsgBuf, BorrowedMsgBufMut, BorrowedOp, FileMode, RawBuf, RawBufMut,
  ReadFlags, RecvFlags, SendFlags, SockProto,
};

const READ: OpenFlags = OpenFlags::READ;

fn backend() -> Wasi {
  let mut backend = Wasi::new();
  backend.init(32).unwrap();
  backend
}

fn push(backend: &mut Wasi, id: u64, op: BorrowedOp<'_>) {
  // SAFETY: every buffer these tests submit outlives the `wait` that
  // returns its completion, or the backend.
  backend.push(unsafe { InFlight::attest(id, op) }, &mut Bump::new());
}

/// Completions ready now, as `(id, result)`, ordered by id.
fn poll(backend: &mut Wasi) -> Vec<(u64, isize)> {
  wait(backend, Some(Duration::ZERO))
}

fn wait(backend: &mut Wasi, timeout: Option<Duration>) -> Vec<(u64, isize)> {
  let mut completed = Vec::new();
  backend.wait(timeout, &mut completed).unwrap();
  let mut done: Vec<(u64, isize)> =
    completed.iter().map(|c| (c.registration_id(), c.result())).collect();
  done.sort_unstable();
  done
}

/// Shuts the backend down the way a dropped driver does: cancels what is
/// not reported yet, then waits, until everything is. Returns what was
/// reported, ordered by id.
fn shut_down(backend: &mut Wasi, ids: &[u64]) -> Vec<(u64, isize)> {
  let mut done: Vec<(u64, isize)> = Vec::new();
  for _ in 0..64 {
    for &id in ids {
      if !done.iter().any(|(reported, _)| *reported == id) {
        backend.cancel(id);
      }
    }
    backend.flush().unwrap();
    done.extend(wait(backend, Some(Duration::from_millis(10))));
    if done.len() >= ids.len() {
      break;
    }
  }
  done.sort_unstable();
  done
}

/// Flushes and waits until `id` completes. Returns its result.
fn finish(backend: &mut Wasi, id: u64) -> isize {
  backend.flush().unwrap();
  loop {
    let done = wait(backend, None);
    assert!(!done.is_empty(), "nothing in flight, but {id} never completed");
    if let Some((_, result)) = done.iter().find(|(done, _)| *done == id) {
      assert_eq!(done.len(), 1, "an unexpected completion: {done:?}");
      return *result;
    }
  }
}

/// Runs one operation to its result.
fn run(backend: &mut Wasi, op: BorrowedOp<'_>) -> isize {
  push(backend, 1, op);
  finish(backend, 1)
}

/// Owns a resource the backend returned, closed the backend's way.
fn own(backend: &Wasi, raw: isize) -> Resource {
  assert!(raw >= FIRST_ID as isize, "not a new resource: {raw}");
  let done = Done::new(raw).with_resource_close(backend.resource_close());
  // SAFETY: `raw` is a fresh resource this backend just returned.
  unsafe { Resource::from_completion(&done) }.unwrap()
}

fn mode() -> FileMode {
  FileMode::from_octal(0o644).unwrap()
}

/// A fresh directory under the preopened root, named for one test.
fn scratch(test: &str) -> String {
  let path = format!("/lio-wasi-{test}");
  let _ = std::fs::remove_dir_all(&path);
  std::fs::create_dir(&path).unwrap();
  path
}

fn open(backend: &mut Wasi, path: &str, flags: OpenFlags) -> isize {
  run(backend, BorrowedOp::open_at(Resource::cwd(), path.into(), flags, mode()))
}

fn loopback() -> SocketAddr {
  ([127, 0, 0, 1], 0).into()
}

fn local(socket: &Resource) -> SocketAddr {
  with_table(|table| match table.entry(socket.as_raw_fd()).unwrap() {
    Entry::Tcp(tcp) => from_wasi(tcp.socket.get_local_address().unwrap()),
    Entry::Udp(udp) => from_wasi(udp.get_local_address().unwrap()),
    Entry::File(_) => panic!("not a socket"),
  })
}

fn tcp_socket(backend: &mut Wasi, domain: SockDomain) -> Resource {
  let raw =
    run(backend, BorrowedOp::socket(domain, SockType::STREAM, SockProto::TCP));
  own(backend, raw)
}

/// A socket listening on `at`, and the address it got.
fn listener(backend: &mut Wasi, at: SocketAddr) -> (Resource, SocketAddr) {
  let domain = if at.is_ipv4() { SockDomain::IPV4 } else { SockDomain::IPV6 };
  let socket = tcp_socket(backend, domain);
  let bound =
    run(backend, BorrowedOp::bind(socket.clone(), socket_addr_into_buf(at)));
  assert_eq!(bound, 0);
  assert_eq!(run(backend, BorrowedOp::listen(socket.clone(), 8)), 0);
  let addr = local(&socket);
  (socket, addr)
}

/// A connected pair: the listener, the client's end and the server's end.
fn pair(backend: &mut Wasi, at: SocketAddr) -> (Resource, Resource, Resource) {
  let (listener, addr) = listener(backend, at);
  let domain = if at.is_ipv4() { SockDomain::IPV4 } else { SockDomain::IPV6 };
  let client = tcp_socket(backend, domain);
  let mut peer = SocketAddrBuf::unspecified();
  push(backend, 1, BorrowedOp::accept(listener.clone(), &mut peer));
  push(
    backend,
    2,
    BorrowedOp::connect(client.clone(), socket_addr_into_buf(addr)),
  );
  backend.flush().unwrap();
  let mut done = Vec::new();
  while done.len() < 2 {
    done.extend(wait(backend, None));
  }
  done.sort_unstable();
  assert_eq!(done[1], (2, 0), "connect failed");
  let server = own(backend, done[0].1);
  (listener, client, server)
}

fn send(backend: &mut Wasi, to: &Resource, data: &[u8]) -> isize {
  let bufs = [BorrowedMsgBuf::from_slice(data)];
  run(backend, BorrowedOp::send(to.clone(), &bufs, None, SendFlags::empty()))
}

// ───────────────────────────── queue semantics ──────────────────────────────

#[test]
fn an_idle_backend_has_no_completions_and_does_not_block() {
  let mut backend = backend();
  backend.flush().unwrap();
  assert!(poll(&mut backend).is_empty());
  // Nothing is in flight, so there is nothing to block on.
  assert!(wait(&mut backend, None).is_empty());
}

#[test]
fn pushed_work_is_not_observable_until_flush() {
  let mut backend = backend();
  push(&mut backend, 7, BorrowedOp::nop());
  assert!(poll(&mut backend).is_empty());
  backend.flush().unwrap();
  assert_eq!(poll(&mut backend), [(7, 0)]);
}

#[test]
fn a_completion_is_delivered_once() {
  let mut backend = backend();
  push(&mut backend, 1, BorrowedOp::nop());
  backend.flush().unwrap();
  assert_eq!(poll(&mut backend), [(1, 0)]);
  assert!(poll(&mut backend).is_empty());
}

#[test]
fn a_batch_completes_each_registration_under_its_own_id() {
  let mut backend = backend();
  let root = scratch("batch");
  push(&mut backend, 1, BorrowedOp::nop());
  push(
    &mut backend,
    2,
    BorrowedOp::mkdir_at(
      Resource::cwd(),
      format!("{root}/made").into(),
      mode(),
    ),
  );
  push(
    &mut backend,
    3,
    BorrowedOp::open_at(
      Resource::cwd(),
      format!("{root}/absent").into(),
      READ,
      mode(),
    ),
  );
  backend.flush().unwrap();
  let mut done = Vec::new();
  while done.len() < 3 {
    done.extend(wait(&mut backend, None));
  }
  done.sort_unstable();
  assert_eq!(done, [(1, 0), (2, 0), (3, fail(libc::ENOENT))]);
  assert!(std::fs::metadata(format!("{root}/made")).unwrap().is_dir());
}

#[test]
fn a_bounded_wait_with_nothing_ready_returns_after_its_timeout() {
  let mut backend = backend();
  let (listening, _) = listener(&mut backend, loopback());
  let mut peer = SocketAddrBuf::unspecified();
  push(&mut backend, 5, BorrowedOp::accept(listening.clone(), &mut peer));
  backend.flush().unwrap();

  let start = Instant::now();
  assert!(wait(&mut backend, Some(Duration::from_millis(40))).is_empty());
  let waited = start.elapsed();
  assert!(waited >= Duration::from_millis(40), "returned after {waited:?}");
  assert!(waited < Duration::from_secs(2), "returned after {waited:?}");

  // The accept is still in flight, and the timer left nothing behind.
  assert!(poll(&mut backend).is_empty());
  backend.cancel(5);
  assert_eq!(wait(&mut backend, None), [(5, CANCELLED)]);
}

// ──────────────────────────────── results ───────────────────────────────────

#[test]
fn file_data_round_trips_and_matches_what_std_sees() {
  let mut backend = backend();
  let root = scratch("round-trip");
  let path = format!("{root}/data");
  let flags = OpenFlags::READ | OpenFlags::WRITE | OpenFlags::CREATE;
  let raw = open(&mut backend, &path, flags);
  let file = own(&backend, raw);

  let (head, tail) = (b"hello, ".as_slice(), b"wasi".as_slice());
  let parts = [RawBuf::new(head), RawBuf::new(tail)];
  let written = run(
    &mut backend,
    BorrowedOp::write(file.clone(), &parts, Some(0), WriteFlags::empty()),
  );
  assert_eq!(written, 11);
  assert_eq!(std::fs::read(&path).unwrap(), b"hello, wasi");

  let (mut first, mut second) = ([0u8; 5], [0u8; 16]);
  let bufs =
    [RawBufMut::from_mut(&mut first), RawBufMut::from_mut(&mut second)];
  let read = run(
    &mut backend,
    BorrowedOp::read(file.clone(), &bufs, Some(0), ReadFlags::empty()),
  );
  assert_eq!(read, 11);
  assert_eq!(&first, b"hello");
  assert_eq!(&second[..6], b", wasi");

  // A read at the end of the file is the end, not an error.
  let mut more = [0u8; 4];
  let bufs = [RawBufMut::from_mut(&mut more)];
  let read = run(
    &mut backend,
    BorrowedOp::read(file, &bufs, Some(11), ReadFlags::empty()),
  );
  assert_eq!(read, 0);
}

#[test]
fn a_synced_write_reports_its_byte_count() {
  let mut backend = backend();
  let root = scratch("synced");
  let flags = OpenFlags::WRITE | OpenFlags::CREATE;
  let raw = open(&mut backend, &format!("{root}/data"), flags);
  let file = own(&backend, raw);
  for sync in [WriteFlags::SYNC, WriteFlags::DSYNC] {
    let parts = [RawBuf::new(b"durable")];
    let written =
      run(&mut backend, BorrowedOp::write(file.clone(), &parts, Some(0), sync));
    assert_eq!(written, 7);
  }
}

#[test]
fn an_offset_past_the_limit_is_invalid() {
  let mut backend = backend();
  let root = scratch("offset");
  let flags = OpenFlags::READ | OpenFlags::WRITE | OpenFlags::CREATE;
  let raw = open(&mut backend, &format!("{root}/data"), flags);
  let file = own(&backend, raw);
  let past = Some(crate::backend::op::MAX_FILE_OFFSET + 1);

  let mut buf = [0u8; 4];
  let bufs = [RawBufMut::from_mut(&mut buf)];
  let read = run(
    &mut backend,
    BorrowedOp::read(file.clone(), &bufs, past, ReadFlags::empty()),
  );
  assert_eq!(read, fail(libc::EINVAL));
  let parts = [RawBuf::new(b"data")];
  let written = run(
    &mut backend,
    BorrowedOp::write(file, &parts, past, WriteFlags::empty()),
  );
  assert_eq!(written, fail(libc::EINVAL));
}

#[test]
fn an_invalid_resource_is_a_bad_descriptor() {
  let mut backend = backend();
  let mut buf = [0u8; 4];
  let bufs = [RawBufMut::from_mut(&mut buf)];
  let read = run(
    &mut backend,
    BorrowedOp::read(Resource::invalid(), &bufs, None, ReadFlags::empty()),
  );
  assert_eq!(read, fail(libc::EBADF));
  let parts = [RawBuf::new(b"data")];
  let written = run(
    &mut backend,
    BorrowedOp::write(Resource::invalid(), &parts, None, WriteFlags::empty()),
  );
  assert_eq!(written, fail(libc::EBADF));
  assert_eq!(
    run(&mut backend, BorrowedOp::fsync(Resource::invalid())),
    fail(libc::EBADF)
  );
  assert_eq!(
    run(&mut backend, BorrowedOp::listen(Resource::invalid(), 1)),
    fail(libc::EBADF)
  );
}

#[test]
fn flags_a_stream_cannot_honour_are_refused() {
  let mut backend = backend();
  let parts = [RawBuf::new(b"x")];
  let written = run(
    &mut backend,
    BorrowedOp::write(Resource::stdout(), &parts, None, WriteFlags::SYNC),
  );
  assert_eq!(written, fail(libc::ENOTSUP));

  let (_listener, client, _server) = pair(&mut backend, loopback());
  let mut buf = [0u8; 4];
  let bufs = [BorrowedMsgBufMut::from_slice(&mut buf)];
  let received =
    run(&mut backend, BorrowedOp::recv(client, &bufs, RecvFlags::PEEK));
  assert_eq!(received, fail(libc::ENOTSUP));
}

#[test]
fn path_errors_keep_their_code() {
  let mut backend = backend();
  let root = scratch("paths");
  let missing = format!("{root}/absent");
  assert_eq!(open(&mut backend, &missing, READ), fail(libc::ENOENT));
  let made = || {
    BorrowedOp::mkdir_at(Resource::cwd(), format!("{root}/d").into(), mode())
  };
  assert_eq!(run(&mut backend, made()), 0);
  assert_eq!(run(&mut backend, made()), fail(libc::EEXIST));
  let not_utf8 = {
    use std::os::wasi::ffi::OsStringExt;
    OsString::from_vec(vec![b'/', 0xff, 0xfe])
  };
  let opened = run(
    &mut backend,
    BorrowedOp::open_at(Resource::cwd(), not_utf8, READ, mode()),
  );
  assert_eq!(opened, fail(libc::EILSEQ));
}

#[test]
fn a_unix_socket_is_an_unsupported_family() {
  let mut backend = backend();
  let made = run(
    &mut backend,
    BorrowedOp::socket(SockDomain::UNIX, SockType::STREAM, SockProto::DEFAULT),
  );
  assert_eq!(made, fail(libc::EAFNOSUPPORT));
  let mismatched = run(
    &mut backend,
    BorrowedOp::socket(SockDomain::IPV4, SockType::STREAM, SockProto::UDP),
  );
  assert_eq!(mismatched, fail(libc::EINVAL));
}

#[test]
fn cursor_reads_in_one_batch_each_get_their_own_part() {
  let mut backend = backend();
  let root = scratch("cursor-batch");
  let path = format!("{root}/data");
  std::fs::write(&path, b"abcdef").unwrap();
  let raw = open(&mut backend, &path, READ);
  let file = own(&backend, raw);

  let (mut first, mut second) = ([0u8; 3], [0u8; 3]);
  let first_bufs = [RawBufMut::from_mut(&mut first)];
  let second_bufs = [RawBufMut::from_mut(&mut second)];
  push(
    &mut backend,
    1,
    BorrowedOp::read(file.clone(), &first_bufs, None, ReadFlags::empty()),
  );
  push(
    &mut backend,
    2,
    BorrowedOp::read(file.clone(), &second_bufs, None, ReadFlags::empty()),
  );
  backend.flush().unwrap();
  let mut done = Vec::new();
  while done.len() < 2 {
    done.extend(wait(&mut backend, None));
  }
  done.sort_unstable();
  assert_eq!(done, [(1, 3), (2, 3)]);
  assert_eq!((&first, &second), (b"abc", b"def"));
}

#[test]
fn a_read_with_its_own_offset_does_not_wait_for_the_cursor() {
  let mut backend = backend();
  let root = scratch("offset-skips-lane");
  let path = format!("{root}/data");
  std::fs::write(&path, b"abcdef").unwrap();
  let raw = open(&mut backend, &path, READ);
  let file = own(&backend, raw);
  assert!(
    Wasi::lane_of(
      BorrowedOp::read(file.clone(), &[], Some(0), ReadFlags::empty()).as_op()
    )
    .is_none()
  );
  assert_eq!(
    Wasi::lane_of(
      BorrowedOp::read(file.clone(), &[], None, ReadFlags::empty()).as_op()
    ),
    Some((file.as_raw_fd(), Way::Cursor))
  );
}

#[test]
fn an_append_leaves_the_cursor_at_the_new_end() {
  let mut backend = backend();
  let root = scratch("append-cursor");
  let path = format!("{root}/log");
  std::fs::write(&path, b"12345").unwrap();
  let raw = open(&mut backend, &path, OpenFlags::READ | OpenFlags::APPEND);
  let file = own(&backend, raw);

  let parts = [RawBuf::new(b"67")];
  let written = run(
    &mut backend,
    BorrowedOp::write(file.clone(), &parts, None, WriteFlags::empty()),
  );
  assert_eq!(written, 2);
  assert_eq!(std::fs::read(&path).unwrap(), b"1234567");
  let cursor = with_table(|table| table.file(file.as_raw_fd()).unwrap().cursor);
  assert_eq!(cursor, 7, "the cursor is at the end the append made");

  // So a read at the cursor finds nothing left.
  let mut buf = [0u8; 8];
  let bufs = [RawBufMut::from_mut(&mut buf)];
  let read =
    run(&mut backend, BorrowedOp::read(file, &bufs, None, ReadFlags::empty()));
  assert_eq!(read, 0);
}

#[test]
fn a_synced_append_still_moves_the_cursor() {
  let mut backend = backend();
  let root = scratch("append-synced");
  let path = format!("{root}/log");
  std::fs::write(&path, b"abc").unwrap();
  let raw = open(&mut backend, &path, OpenFlags::READ | OpenFlags::WRITE);
  let file = own(&backend, raw);
  let parts = [RawBuf::new(b"de")];
  let flags = WriteFlags::APPEND | WriteFlags::SYNC;
  let written =
    run(&mut backend, BorrowedOp::write(file.clone(), &parts, None, flags));
  assert_eq!(written, 2);
  assert_eq!(std::fs::read(&path).unwrap(), b"abcde");
  let cursor = with_table(|table| table.file(file.as_raw_fd()).unwrap().cursor);
  assert_eq!(cursor, 5);
}

// ─────────────────────────────── cancellation ───────────────────────────────

#[test]
fn cancelling_before_flush_completes_as_cancelled() {
  let mut backend = backend();
  push(&mut backend, 3, BorrowedOp::nop());
  backend.cancel(3);
  backend.flush().unwrap();
  assert_eq!(poll(&mut backend), [(3, CANCELLED)]);
  assert!(poll(&mut backend).is_empty());
}

#[test]
fn cancelling_an_unknown_id_does_nothing() {
  let mut backend = backend();
  backend.cancel(99);
  backend.flush().unwrap();
  assert!(poll(&mut backend).is_empty());
}

#[test]
fn a_cancelled_accept_completes_once_and_the_listener_still_accepts() {
  let mut backend = backend();
  let (listening, addr) = listener(&mut backend, loopback());
  let mut peer = SocketAddrBuf::unspecified();
  push(&mut backend, 4, BorrowedOp::accept(listening.clone(), &mut peer));
  backend.flush().unwrap();
  assert!(poll(&mut backend).is_empty());
  backend.cancel(4);
  assert_eq!(wait(&mut backend, None), [(4, CANCELLED)]);
  assert!(poll(&mut backend).is_empty());

  let client = tcp_socket(&mut backend, SockDomain::IPV4);
  push(&mut backend, 5, BorrowedOp::accept(listening, &mut peer));
  push(
    &mut backend,
    6,
    BorrowedOp::connect(client, socket_addr_into_buf(addr)),
  );
  backend.flush().unwrap();
  let mut done = Vec::new();
  while done.len() < 2 {
    done.extend(wait(&mut backend, None));
  }
  done.sort_unstable();
  assert!(done[0].1 >= FIRST_ID as isize, "accept failed: {done:?}");
  assert_eq!(done[1], (6, 0));
  drop(own(&backend, done[0].1));
}

#[test]
fn a_cancelled_receive_leaves_the_socket_usable() {
  let mut backend = backend();
  let (_listener, client, server) = pair(&mut backend, loopback());
  let mut buf = [0u8; 8];
  {
    let bufs = [BorrowedMsgBufMut::from_slice(&mut buf)];
    push(
      &mut backend,
      9,
      BorrowedOp::recv(server.clone(), &bufs, RecvFlags::empty()),
    );
    backend.flush().unwrap();
    assert!(poll(&mut backend).is_empty());
    backend.cancel(9);
    assert_eq!(wait(&mut backend, None), [(9, CANCELLED)]);
  }
  assert_eq!(send(&mut backend, &client, b"after"), 5);
  let bufs = [BorrowedMsgBufMut::from_slice(&mut buf)];
  let received =
    run(&mut backend, BorrowedOp::recv(server, &bufs, RecvFlags::empty()));
  assert_eq!(received, 5);
  assert_eq!(&buf[..5], b"after");
}

#[test]
fn cancelling_a_receive_waiting_its_turn_leaves_the_one_ahead() {
  let mut backend = backend();
  let (_listener, client, server) = pair(&mut backend, loopback());
  let (mut first, mut second) = ([0u8; 8], [0u8; 8]);
  let first_bufs = [BorrowedMsgBufMut::from_slice(&mut first)];
  let second_bufs = [BorrowedMsgBufMut::from_slice(&mut second)];
  push(
    &mut backend,
    1,
    BorrowedOp::recv(server.clone(), &first_bufs, RecvFlags::empty()),
  );
  push(
    &mut backend,
    2,
    BorrowedOp::recv(server.clone(), &second_bufs, RecvFlags::empty()),
  );
  backend.flush().unwrap();
  backend.cancel(2);
  assert_eq!(poll(&mut backend), [(2, CANCELLED)]);

  let bufs = [BorrowedMsgBuf::from_slice(b"one")];
  push(
    &mut backend,
    3,
    BorrowedOp::send(client, &bufs, None, SendFlags::empty()),
  );
  backend.flush().unwrap();
  let mut done = Vec::new();
  while done.len() < 2 {
    done.extend(wait(&mut backend, None));
  }
  done.sort_unstable();
  assert_eq!(done, [(1, 3), (3, 3)]);
}

#[test]
fn cancelling_everything_stops_everything_in_flight() {
  let mut backend = backend();
  let (listening, _) = listener(&mut backend, loopback());
  let (_listener, _client, server) = pair(&mut backend, loopback());
  let mut peer = SocketAddrBuf::unspecified();
  let (mut first, mut second) = ([0u8; 8], [0u8; 8]);
  let first_bufs = [BorrowedMsgBufMut::from_slice(&mut first)];
  let second_bufs = [BorrowedMsgBufMut::from_slice(&mut second)];
  push(&mut backend, 1, BorrowedOp::accept(listening, &mut peer));
  push(
    &mut backend,
    2,
    BorrowedOp::recv(server.clone(), &first_bufs, RecvFlags::empty()),
  );
  // Waits its turn behind the receive above.
  push(
    &mut backend,
    3,
    BorrowedOp::recv(server, &second_bufs, RecvFlags::empty()),
  );
  backend.flush().unwrap();
  push(&mut backend, 4, BorrowedOp::nop());

  let done = shut_down(&mut backend, &[1, 2, 3, 4]);
  assert_eq!(
    done,
    [(1, CANCELLED), (2, CANCELLED), (3, CANCELLED), (4, CANCELLED)]
  );
  assert!(backend.ops.is_empty() && backend.queued.is_empty());
  assert_eq!(backend.steps.outstanding(), 0);
  assert!(poll(&mut backend).is_empty(), "each is reported once");
}

#[test]
fn dropping_the_backend_stops_what_is_in_flight() {
  let mut backend = backend();
  let (listening, _) = listener(&mut backend, loopback());
  let (_listener, _client, server) = pair(&mut backend, loopback());
  let mut peer = SocketAddrBuf::unspecified();
  let mut buf = [0u8; 8];
  let bufs = [BorrowedMsgBufMut::from_slice(&mut buf)];
  push(&mut backend, 1, BorrowedOp::accept(listening, &mut peer));
  push(&mut backend, 2, BorrowedOp::recv(server, &bufs, RecvFlags::empty()));
  backend.flush().unwrap();
  assert!(poll(&mut backend).is_empty());
  // The host is told to stop before the operations' memory goes away.
  drop(backend);
  assert!(ORPHANS.with(|orphans| orphans.borrow().is_empty()));
}

#[test]
fn cancelling_follows_operations_through_their_later_steps() {
  let mut backend = backend();
  let root = scratch("drain-steps");
  let path = format!("{root}/log");
  std::fs::write(&path, b"abc").unwrap();
  let flags = OpenFlags::READ | OpenFlags::WRITE;
  let raw = open(&mut backend, &path, flags);
  let file = own(&backend, raw);

  // Each of these has steps after its first: a status, a sync, the size.
  let parts = [RawBuf::new(b"de")];
  let synced_append = WriteFlags::APPEND | WriteFlags::SYNC;
  let (mut first, mut second) = ([0u8; 2], [0u8; 2]);
  let bufs =
    [RawBufMut::from_mut(&mut first), RawBufMut::from_mut(&mut second)];
  push(
    &mut backend,
    1,
    BorrowedOp::write(file.clone(), &parts, None, synced_append),
  );
  push(
    &mut backend,
    2,
    BorrowedOp::read(file.clone(), &bufs, Some(0), ReadFlags::empty()),
  );
  backend.flush().unwrap();

  // Each is reported once, stopped or finished, whichever came first.
  let done = shut_down(&mut backend, &[1, 2]);
  let ids: Vec<u64> = done.iter().map(|(id, _)| *id).collect();
  assert_eq!(ids, [1, 2]);
  assert!(backend.ops.is_empty());
  assert_eq!(backend.steps.outstanding(), 0, "the host still holds a step");
  assert!(poll(&mut backend).is_empty());
}

// ─────────────────────────────── resources ──────────────────────────────────

#[test]
fn closing_a_listener_frees_its_port() {
  let mut backend = backend();
  let (listening, addr) = listener(&mut backend, loopback());
  // The listening stream is in this backend's set, so the close is
  // finished by the backend the next time it runs.
  drop(listening);
  backend.flush().unwrap();
  let client = tcp_socket(&mut backend, SockDomain::IPV4);
  let refused =
    run(&mut backend, BorrowedOp::connect(client, socket_addr_into_buf(addr)));
  assert_eq!(refused, fail(libc::ECONNREFUSED));
}

#[test]
fn a_closed_resource_id_is_reused_and_the_old_one_is_gone() {
  let mut backend = backend();
  let root = scratch("reuse");
  let flags = OpenFlags::WRITE | OpenFlags::CREATE;
  let first = open(&mut backend, &format!("{root}/a"), flags);
  drop(own(&backend, first));
  let second = open(&mut backend, &format!("{root}/b"), flags);
  assert_eq!(first, second, "the freed id was not reused");
  drop(own(&backend, second));
  assert!(with_table(|table| table.entry(second as RawResource).is_err()));
}

#[test]
fn a_socket_from_another_backend_is_refused() {
  let mut ours = backend();
  let mut theirs = backend();
  let (_listener, client, _server) = pair(&mut theirs, loopback());
  assert_eq!(send(&mut ours, &client, b"x"), fail(libc::EBADF));
  assert_eq!(send(&mut theirs, &client, b"x"), 1);
}

#[test]
fn ipv6_connects_and_carries_data() {
  let mut backend = backend();
  let at = SocketAddr::from((Ipv6Addr::LOCALHOST, 0));
  let (_listener, client, server) = pair(&mut backend, at);
  assert!(local(&client).is_ipv6());
  assert_eq!(send(&mut backend, &client, b"six"), 3);
  let mut buf = [0u8; 8];
  let bufs = [BorrowedMsgBufMut::from_slice(&mut buf)];
  let received =
    run(&mut backend, BorrowedOp::recv(server, &bufs, RecvFlags::empty()));
  assert_eq!(received, 3);
  assert_eq!(&buf[..3], b"six");
}

#[test]
fn a_peer_that_closes_ends_the_stream_and_breaks_the_pipe() {
  let mut backend = backend();
  let (_listener, client, server) = pair(&mut backend, loopback());
  drop(client);
  backend.flush().unwrap();
  let mut buf = [0u8; 8];
  let bufs = [BorrowedMsgBufMut::from_slice(&mut buf)];
  let received = run(
    &mut backend,
    BorrowedOp::recv(server.clone(), &bufs, RecvFlags::empty()),
  );
  assert_eq!(received, 0, "the peer's close is the end of the stream");
  // A second receive finds the end again, without a status left to read.
  let received =
    run(&mut backend, BorrowedOp::recv(server, &bufs, RecvFlags::empty()));
  assert_eq!(received, 0);
}

#[test]
fn a_directory_closed_mid_listing_is_cleaned_up() {
  let mut backend = backend();
  let root = scratch("mid-listing");
  for name in ["a", "b", "c"] {
    std::fs::write(format!("{root}/{name}"), b"").unwrap();
  }
  let raw = open(&mut backend, &root, OpenFlags::READ | OpenFlags::DIRECTORY);
  let dir = own(&backend, raw);
  let (mut names, mut entries) = ([0u8; 64], [DirEntryRef::default(); 1]);
  let mut result = ReadDirResult::default();
  let (mut opaque, mut opaque_drop) = (std::ptr::null_mut(), None);
  let listed = run(
    &mut backend,
    BorrowedOp::read_dir(
      dir.clone(),
      &mut names,
      &mut entries,
      &mut result,
      &mut opaque,
      &mut opaque_drop,
    ),
  );
  assert_eq!(listed, 0);
  assert_eq!((result.entries, result.eof), (1, false));
  assert!(opaque.is_null(), "the listing lives with the directory");

  // Its stream is still open in the set; the backend closes it.
  drop(dir);
  backend.flush().unwrap();
  assert!(ORPHANS.with(|orphans| orphans.borrow().is_empty()));
  assert_eq!(backend.steps.outstanding(), 0);
}
