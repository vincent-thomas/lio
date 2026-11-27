//! Operation models: single operations, composites, parallel I/O
//! and cancellation, without real I/O where the order of completions matters.

use lio::ready;
use std::cell::RefCell;
use std::ffi::CString;
use std::future::Future;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

use lio::api::ops::{Read, Write};
use lio::api::resource::Resource;
use lio::api::{self, Io, OpenFlags};
use lio::backend::ds::{DSBackend, DSConfig};
use lio::backend::op::Op;
use lio::backend::testing::{Checked, POISON, ScriptedBackend};
use lio::model::{Cx, Join, Next, OpModel, OpStream, Step, Task};
use lio::{BufResult, Lio};

/// A read into a `Vec`, as `api::read` returns it.
type ReadOp = Io<Read<Vec<u8>>>;

fn scripted(cap: usize) -> (ScriptedBackend, Lio) {
  let backend = ScriptedBackend::new();
  let lio = Lio::new_with_backend(Checked::new(backend.clone()), cap).unwrap();
  (backend, lio)
}

/// Spawns `model` and collects every item it produces.
fn collect<M>(lio: &Lio, model: M) -> (Rc<RefCell<Vec<M::Item>>>, Task)
where
  M: OpModel + 'static,
{
  let items = Rc::new(RefCell::new(Vec::new()));
  let sink = Rc::clone(&items);
  let task = api::run(model)
    .with_lio(lio)
    .when_done(move |item| sink.borrow_mut().push(item));
  (items, task)
}

/// Finishes the only operation in flight with `result`, and lets the driver
/// route it.
fn finish_only(backend: &ScriptedBackend, lio: &Lio, result: isize) {
  lio.try_run().unwrap();
  let in_flight = backend.in_flight();
  assert_eq!(in_flight.len(), 1, "expected one operation in flight");
  backend.complete(in_flight[0], result);
  lio.try_run().unwrap();
}

fn only_op(backend: &ScriptedBackend, lio: &Lio) -> Op {
  lio.try_run().unwrap();
  let in_flight = backend.in_flight();
  assert_eq!(in_flight.len(), 1, "expected one operation in flight");
  backend.op(in_flight[0]).unwrap()
}

#[test]
fn read_gives_back_the_buffer_with_what_was_written() {
  let (backend, lio) = scripted(4);
  let stdin = Resource::stdin();
  // Uninitialized capacity: only the bytes the backend wrote come back.
  let (items, _task) =
    collect(&lio, api::read(&stdin, Vec::<u8>::with_capacity(8)));

  assert!(items.borrow().is_empty());
  finish_only(&backend, &lio, 3);

  let mut items = items.borrow_mut();
  let (result, buf) = items.pop().unwrap();
  assert_eq!(result.unwrap(), 3);
  assert_eq!(buf, vec![POISON; 3]);
  assert!(items.is_empty());
}

/// The in-flight operations `is` accepts.
fn in_flight(backend: &ScriptedBackend, is: impl Fn(&Op) -> bool) -> Vec<u64> {
  let mut ids = backend.in_flight();
  ids.retain(|&id| backend.op(id).is_some_and(|op| is(&op)));
  ids
}

fn reads(backend: &ScriptedBackend) -> Vec<u64> {
  in_flight(backend, |op| matches!(op, Op::Read { .. }))
}

fn writes(backend: &ScriptedBackend) -> Vec<u64> {
  in_flight(backend, |op| matches!(op, Op::Write { .. }))
}

/// Finishes `id` with `result`, and lets the driver route it and submit
/// what that starts.
fn finish(backend: &ScriptedBackend, lio: &Lio, id: u64, result: isize) {
  backend.complete(id, result);
  lio.try_run().unwrap();
  lio.try_run().unwrap();
}

#[test]
fn copy_reads_the_next_chunk_while_it_writes_the_last() {
  let (backend, lio) = scripted(4);
  let (stdin, stdout) = (Resource::stdin(), Resource::stdout());
  let (items, _task) = collect(&lio, lio::io::copy(&stdin, &stdout));
  lio.try_run().unwrap();
  assert_eq!((reads(&backend).len(), writes(&backend).len()), (1, 0));

  // The first chunk is written while the second is read.
  finish(&backend, &lio, reads(&backend)[0], 5);
  assert_eq!((reads(&backend).len(), writes(&backend).len()), (1, 1));

  // A short write: the rest of the chunk is written again.
  finish(&backend, &lio, writes(&backend)[0], 2);
  assert_eq!((reads(&backend).len(), writes(&backend).len()), (1, 1));

  // The second chunk arrives while the first is still being written. It
  // waits, and nothing more is read until it has been taken.
  finish(&backend, &lio, reads(&backend)[0], 4);
  assert_eq!((reads(&backend).len(), writes(&backend).len()), (0, 1));

  // The first chunk is out: the second is written and a third is read.
  finish(&backend, &lio, writes(&backend)[0], 3);
  assert_eq!((reads(&backend).len(), writes(&backend).len()), (1, 1));

  // The source ends while the second chunk is still being written.
  finish(&backend, &lio, reads(&backend)[0], 0);
  assert_eq!((reads(&backend).len(), writes(&backend).len()), (0, 1));
  assert!(items.borrow().is_empty(), "the last chunk is not written yet");

  finish(&backend, &lio, writes(&backend)[0], 4);
  assert_eq!(items.borrow_mut().pop().unwrap().unwrap(), 9);
  assert!(backend.in_flight().is_empty());
  assert_eq!(backend.pushed().len(), 6, "three reads and three writes");
}

#[test]
fn copy_n_reads_no_further_than_its_limit() {
  let (backend, lio) = scripted(4);
  let (stdin, stdout) = (Resource::stdin(), Resource::stdout());
  let (items, _task) = collect(&lio, lio::io::copy_n(&stdin, &stdout, 6));
  lio.try_run().unwrap();

  // Four of six: the read that runs beside the write asks for two more.
  finish(&backend, &lio, reads(&backend)[0], 4);
  let ahead = reads(&backend);
  assert_eq!((ahead.len(), writes(&backend).len()), (1, 1));
  finish(&backend, &lio, ahead[0], 2);
  // The limit is reached: no third read, though the source has not ended.
  finish(&backend, &lio, writes(&backend)[0], 4);
  assert_eq!((reads(&backend).len(), writes(&backend).len()), (0, 1));
  finish(&backend, &lio, writes(&backend)[0], 2);

  assert_eq!(items.borrow_mut().pop().unwrap().unwrap(), 6);
  assert_eq!(backend.pushed().len(), 4, "two reads and two writes");

  let (items, _task) = collect(&lio, lio::io::copy_n(&stdin, &stdout, 0));
  assert_eq!(items.borrow_mut().pop().unwrap().unwrap(), 0);
  assert_eq!(backend.pushed().len(), 4, "a limit of zero submits nothing");
}

#[test]
fn copy_stops_at_the_first_error() {
  let (backend, lio) = scripted(4);
  let (stdin, stdout) = (Resource::stdin(), Resource::stdout());
  let (items, _task) = collect(&lio, lio::io::copy(&stdin, &stdout));

  finish_only(&backend, &lio, 4);
  lio.try_run().unwrap();
  let ahead = reads(&backend);
  finish(&backend, &lio, writes(&backend)[0], -9);

  let err = items.borrow_mut().pop().unwrap().unwrap_err();
  assert_eq!(err.raw_os_error(), Some(9));
  assert_eq!(backend.cancelled(), ahead, "the read beside it is cancelled");
  backend.complete_all(-125);
  lio.try_run().unwrap();
  assert!(backend.in_flight().is_empty());
  assert!(!lio.is_full());

  // An error from the read ends the copy too, and cancels the write.
  let (items, _task) = collect(&lio, lio::io::copy(&stdin, &stdout));
  finish_only(&backend, &lio, 4);
  lio.try_run().unwrap();
  let writing = writes(&backend);
  finish(&backend, &lio, reads(&backend)[0], -5);
  let err = items.borrow_mut().pop().unwrap().unwrap_err();
  assert_eq!(err.raw_os_error(), Some(5));
  assert!(backend.cancelled().ends_with(&writing));
  backend.complete_all(-125);
  lio.try_run().unwrap();
  assert!(!lio.is_full());
}

#[test]
fn join_has_both_operations_in_flight_and_waits_for_both() {
  let (backend, lio) = scripted(4);
  let stdin = Resource::stdin();
  let join = Join::new(
    api::read(&stdin, Vec::<u8>::with_capacity(8)),
    api::read(&stdin, Vec::<u8>::with_capacity(8)),
  );
  let (items, _task) = collect(&lio, join);
  lio.try_run().unwrap();

  let in_flight = backend.in_flight();
  assert_eq!(in_flight.len(), 2);

  // The second finishes first. Each side only sees its own result.
  backend.complete(in_flight[1], 1);
  lio.try_run().unwrap();
  assert!(items.borrow().is_empty());
  backend.complete(in_flight[0], 2);
  lio.try_run().unwrap();

  let ((first, first_buf), (second, second_buf)) =
    items.borrow_mut().pop().unwrap();
  assert_eq!((first.unwrap(), first_buf.len()), (2, 2));
  assert_eq!((second.unwrap(), second_buf.len()), (1, 1));
}

#[test]
fn composites_nest() {
  let (backend, lio) = scripted(4);
  let (stdin, stdout) = (Resource::stdin(), Resource::stdout());
  let model = Join::new(
    lio::io::copy(&stdin, &stdout),
    api::write(&stdout, b"hi".to_vec()),
  );
  let (items, _task) = collect(&lio, model);
  lio.try_run().unwrap();
  assert_eq!(backend.in_flight().len(), 2);

  // Every round finishes whatever is in flight: the copy's read sees 0.
  backend.complete_all(0);
  lio.try_run().unwrap();

  let (copied, (written, buf)) = items.borrow_mut().pop().unwrap();
  assert_eq!(copied.unwrap(), 0);
  assert_eq!(written.unwrap(), 0);
  assert_eq!(buf, b"hi");
}

#[test]
fn cancelling_a_task_cancels_what_it_has_in_flight() {
  let (backend, lio) = scripted(4);
  let stdin = Resource::stdin();
  let join = Join::new(
    api::read(&stdin, Vec::<u8>::with_capacity(8)),
    api::read(&stdin, Vec::<u8>::with_capacity(8)),
  );
  let (items, task) = collect(&lio, join);
  lio.try_run().unwrap();
  let in_flight = backend.in_flight();

  task.cancel();
  assert_eq!(backend.cancelled(), in_flight);

  // The buffers stay with the driver until the backend reports. The backend
  // writes them as it does: under Miri a buffer freed early is caught here.
  backend.complete_all(-125);
  lio.try_run().unwrap();
  assert!(items.borrow().is_empty());
  assert!(backend.in_flight().is_empty());
}

#[test]
fn a_dropped_future_cancels_its_model() {
  let (backend, lio) = scripted(4);
  let stdin = Resource::stdin();
  let read = api::read(&stdin, Vec::<u8>::with_capacity(8)).with_lio(&lio);
  let mut future = Box::pin(std::future::IntoFuture::into_future(read));
  let mut cx = Context::from_waker(Waker::noop());
  assert!(future.as_mut().poll(&mut cx).is_pending());
  lio.try_run().unwrap();
  let in_flight = backend.in_flight();

  drop(future);
  assert_eq!(backend.cancelled(), in_flight);
  backend.complete_all(-125);
  lio.try_run().unwrap();
}

#[test]
fn a_future_resolves_to_the_item() {
  let (backend, lio) = scripted(4);
  let stdin = Resource::stdin();
  let read = api::read(&stdin, Vec::<u8>::with_capacity(8)).with_lio(&lio);
  let mut future = Box::pin(std::future::IntoFuture::into_future(read));
  let mut cx = Context::from_waker(Waker::noop());
  assert!(future.as_mut().poll(&mut cx).is_pending());

  finish_only(&backend, &lio, 4);

  let Poll::Ready((result, buf)) = future.as_mut().poll(&mut cx) else {
    panic!("the read finished");
  };
  assert_eq!((result.unwrap(), buf.len()), (4, 4));
}

/// A stream written against the public API: one read after another, an item
/// per read, until the source ends.
struct Chunks {
  from: Resource,
  read: ReadOp,
  ended: bool,
}

impl OpStream for Chunks {
  type Item = BufResult<usize, Vec<u8>>;

  fn poll_next(&mut self, cx: &mut Cx<'_>) -> Next<Self::Item> {
    if self.ended {
      return Next::End;
    }
    let (result, buf) = ready!(self.read.poll(cx));
    match result {
      Ok(n) if n > 0 => {
        self.read = api::read(&self.from, Vec::<u8>::with_capacity(4));
      }
      // The end of the source, or an error: the last item.
      _ => self.ended = true,
    }
    Next::Item((result, buf))
  }
}

#[test]
fn a_stream_yields_until_it_is_done() {
  let (backend, lio) = scripted(4);
  let from = Resource::stdin();
  let read = api::read(&from, Vec::<u8>::with_capacity(4));
  let items = Rc::new(RefCell::new(Vec::new()));
  let sink = Rc::clone(&items);
  let _task = api::run_stream(Chunks { from, read, ended: false })
    .with_lio(&lio)
    .for_each(move |item| sink.borrow_mut().push(item));

  finish_only(&backend, &lio, 4);
  finish_only(&backend, &lio, 2);
  finish_only(&backend, &lio, 0);

  let lens: Vec<usize> =
    items.borrow().iter().map(|(_, buf)| buf.len()).collect();
  assert_eq!(lens, [4, 2, 0]);
  assert!(backend.in_flight().is_empty());
}

fn open(lio: &Lio, path: &str, flags: OpenFlags) -> Resource {
  let mode = api::FileMode::from_octal(0o644).unwrap();
  let path = CString::new(path).unwrap();
  let mut opened =
    api::openat(&Resource::cwd(), path, flags, mode).with_lio(lio).send();
  for _ in 0..10_000 {
    if let Some(resource) = opened.try_recv() {
      return resource.unwrap();
    }
    lio.try_run().unwrap();
  }
  panic!("the open did not finish");
}

/// Runs the driver until `items` holds one item.
fn run_to_item<T>(lio: &Lio, items: &Rc<RefCell<Vec<T>>>) -> T {
  for _ in 0..10_000 {
    if let Some(item) = items.borrow_mut().pop() {
      return item;
    }
    lio.try_run().unwrap();
  }
  panic!("the model did not finish");
}

/// Writes a whole buffer at an offset, however short each write is.
struct WriteAll {
  to: Resource,
  offset: u64,
  write: Io<Write<Vec<u8>>>,
}

impl WriteAll {
  fn new(to: &Resource, data: Vec<u8>) -> Self {
    Self { to: to.clone(), offset: 0, write: api::write_at(to, data, 0) }
  }
}

impl OpModel for WriteAll {
  type Item = std::io::Result<u64>;

  fn poll(&mut self, cx: &mut Cx<'_>) -> Step<Self::Item> {
    loop {
      let (result, mut buf) = ready!(self.write.poll(cx));
      let written = match result {
        Ok(written) => written,
        Err(err) => return Step::Done(Err(err)),
      };
      self.offset += written as u64;
      buf.drain(..written);
      if buf.is_empty() {
        return Step::Done(Ok(self.offset));
      }
      self.write = api::write_at(&self.to, buf, self.offset);
    }
  }
}

/// Reads a resource from the start until it ends.
struct ReadToEnd {
  from: Resource,
  bytes: Vec<u8>,
  read: ReadOp,
}

impl ReadToEnd {
  fn new(from: &Resource) -> Self {
    let read = api::read_at(from, Vec::with_capacity(512), 0);
    Self { from: from.clone(), bytes: Vec::new(), read }
  }
}

impl OpModel for ReadToEnd {
  type Item = std::io::Result<Vec<u8>>;

  fn poll(&mut self, cx: &mut Cx<'_>) -> Step<Self::Item> {
    loop {
      let (result, mut buf) = ready!(self.read.poll(cx));
      match result {
        Ok(0) => return Step::Done(Ok(std::mem::take(&mut self.bytes))),
        Ok(_) => self.bytes.extend_from_slice(&buf),
        Err(err) => return Step::Done(Err(err)),
      }
      buf.clear();
      self.read = api::read_at(&self.from, buf, self.bytes.len() as u64);
    }
  }
}

#[test]
fn copy_moves_a_file_through_a_simulated_backend() {
  let lio =
    Lio::new_with_backend(DSBackend::with_config(DSConfig::default()), 64)
      .unwrap();
  let create = OpenFlags::READ
    | OpenFlags::WRITE
    | OpenFlags::CREATE
    | OpenFlags::TRUNCATE;
  let data: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();

  let src = open(&lio, "/tmp/lio_model_copy_src", create);
  let (written, _task) = collect(&lio, WriteAll::new(&src, data.clone()));
  assert_eq!(run_to_item(&lio, &written).unwrap(), data.len() as u64);

  let src = open(&lio, "/tmp/lio_model_copy_src", OpenFlags::READ);
  let dst = open(&lio, "/tmp/lio_model_copy_dst", create);
  // The file is larger than the copy buffer: several read/write rounds.
  let (copied, _task) = collect(&lio, lio::io::copy(&src, &dst));
  assert_eq!(run_to_item(&lio, &copied).unwrap(), data.len() as u64);

  let dst = open(&lio, "/tmp/lio_model_copy_dst", OpenFlags::READ);
  let (read_back, _task) = collect(&lio, ReadToEnd::new(&dst));
  assert_eq!(run_to_item(&lio, &read_back).unwrap(), data);
}

#[test]
fn a_step_waiting_for_a_slot_is_never_submitted_once_its_model_is_dropped() {
  let (backend, lio) = scripted(1);
  let stdin = Resource::stdin();
  // One slot: the second read waits in the driver's queue.
  let join = Join::new(
    api::read(&stdin, Vec::<u8>::with_capacity(8)),
    api::read(&stdin, Vec::<u8>::with_capacity(8)),
  );
  let (items, task) = collect(&lio, join);
  lio.try_run().unwrap();
  let first = backend.in_flight();
  assert_eq!(first.len(), 1);

  task.cancel();
  assert_eq!(backend.cancelled(), first);
  backend.complete_all(-125);
  lio.try_run().unwrap();
  lio.try_run().unwrap();

  assert_eq!(backend.pushed(), first, "the queued read never ran");
  assert!(items.borrow().is_empty());
  assert!(!lio.is_full());
}

#[test]
fn a_step_that_left_the_queue_is_cancelled_when_its_model_is_dropped() {
  let (backend, lio) = scripted(1);
  let stdin = Resource::stdin();
  // Takes the only slot, so the model's read has to queue.
  api::nop().with_lio(&lio).when_done(|_| {});
  lio.try_run().unwrap();
  let blocker = backend.in_flight();
  let (items, task) =
    collect(&lio, api::read(&stdin, Vec::<u8>::with_capacity(8)));
  assert_eq!(backend.pushed(), blocker, "the read is waiting for a slot");

  backend.complete_all(0);
  lio.try_run().unwrap();
  lio.try_run().unwrap();
  let read = backend.in_flight();
  assert_eq!(read.len(), 1, "the read took the freed slot");

  task.cancel();
  assert_eq!(backend.cancelled(), read);
  backend.complete_all(-125);
  lio.try_run().unwrap();
  assert!(items.borrow().is_empty());
  assert!(!lio.is_full());
}

#[test]
fn a_model_s_next_step_reuses_the_slot_its_last_one_freed() {
  let (backend, lio) = scripted(1);
  let stdin = Resource::stdin();
  let stdout = Resource::stdout();
  // Read, then write, then read: with one slot, each step needs the slot
  // the step before it held.
  let (items, _task) = collect(&lio, lio::io::copy(&stdin, &stdout));
  finish_only(&backend, &lio, 3);
  assert!(matches!(only_op(&backend, &lio), Op::Write { .. }));
  finish_only(&backend, &lio, 3);
  assert!(matches!(only_op(&backend, &lio), Op::Read { .. }));
  finish_only(&backend, &lio, 0);
  assert_eq!(items.borrow_mut().pop().unwrap().unwrap(), 3);
  assert!(!lio.is_full());
}

/// Polls its read once, then hands it out while it is in flight.
struct Leaks {
  read: Option<ReadOp>,
  out: Rc<RefCell<Option<ReadOp>>>,
}

impl OpModel for Leaks {
  type Item = ();

  fn poll(&mut self, cx: &mut Cx<'_>) -> Step<()> {
    let mut read = self.read.take().unwrap();
    assert!(matches!(read.poll(cx), Step::Pending));
    *self.out.borrow_mut() = Some(read);
    Step::Done(())
  }
}

#[test]
fn an_operation_in_flight_can_move_to_another_model() {
  let (backend, lio) = scripted(2);
  let out = Rc::new(RefCell::new(None));
  let read = api::read(&Resource::stdin(), Vec::with_capacity(8));
  let _task = api::run(Leaks { read: Some(read), out: Rc::clone(&out) })
    .with_lio(&lio)
    .when_done(|()| {});
  // The model that started the read is done. The read is still in flight,
  // in that model's slot, and its ticket is here.
  let in_flight = out.borrow_mut().take().unwrap();
  lio.try_run().unwrap();
  assert_eq!(backend.in_flight().len(), 1);

  // Whoever polls it next is the one told when it finishes.
  let (items, _task) = collect(&lio, in_flight);
  assert_eq!(backend.pushed().len(), 1, "it is not submitted again");
  finish_only(&backend, &lio, 3);
  let (result, buf) = items.borrow_mut().pop().unwrap();
  assert_eq!((result.unwrap(), buf.len()), (3, 3));
  assert!(!lio.is_full(), "both slots are free again");
}

/// Starts two reads, and hands the second out before it is done.
struct HandsOut {
  reads: Option<[ReadOp; 2]>,
  out: Rc<RefCell<Option<ReadOp>>>,
}

impl OpModel for HandsOut {
  type Item = ();

  fn poll(&mut self, cx: &mut Cx<'_>) -> Step<()> {
    let [mut first, mut second] = self.reads.take().unwrap();
    assert!(matches!(first.poll(cx), Step::Pending));
    assert!(matches!(second.poll(cx), Step::Pending));
    *self.out.borrow_mut() = Some(second);
    Step::Done(())
  }
}

/// Waits for a nop, then for the read it finds in `from`.
struct PicksUp {
  nop: Option<Io<lio::api::ops::Nop>>,
  from: Rc<RefCell<Option<ReadOp>>>,
  read: Option<ReadOp>,
}

impl OpModel for PicksUp {
  type Item = BufResult<usize, Vec<u8>>;

  fn poll(&mut self, cx: &mut Cx<'_>) -> Step<Self::Item> {
    if let Some(nop) = self.nop.as_mut() {
      ready!(nop.poll(cx)).unwrap();
      self.nop = None;
      self.read = self.from.borrow_mut().take();
    }
    Step::Done(ready!(self.read.as_mut().unwrap().poll(cx)))
  }
}

#[test]
fn an_operation_waiting_for_a_slot_can_move_to_another_model() {
  let (backend, lio) = scripted(2);
  let out = Rc::new(RefCell::new(None));
  let picks_up =
    PicksUp { nop: Some(api::nop()), from: Rc::clone(&out), read: None };
  let (items, _task) = collect(&lio, picks_up);
  lio.try_run().unwrap();
  let nop = backend.in_flight()[0];

  // The first read fills the driver: the second waits for a slot. The
  // model gives the first up, and the backend is asked to stop it.
  let read = || api::read(&Resource::stdin(), Vec::with_capacity(8));
  let _task =
    api::run(HandsOut { reads: Some([read(), read()]), out: Rc::clone(&out) })
      .with_lio(&lio)
      .when_done(|()| {});
  assert_eq!(backend.pushed().len(), 2);
  let given_up = reads(&backend)[0];
  assert_eq!(backend.cancelled(), [given_up]);

  // The other model takes the waiting read over, and asks for its result
  // before it has a slot. The read its first model gave up on then leaves
  // its slot, which the waiting read gets.
  finish(&backend, &lio, nop, 0);
  assert!(out.borrow().is_none());
  finish(&backend, &lio, given_up, -1);
  assert_eq!(backend.pushed().len(), 3, "it has a slot now");

  // The model that has the ticket now is the one told.
  finish(&backend, &lio, reads(&backend)[0], 3);
  let (result, buf) = items.borrow_mut().pop().unwrap();
  assert_eq!((result.unwrap(), buf.len()), (3, 3));
  assert!(!lio.is_full());
}

/// Two nops, one after the other.
struct TwoNops {
  left: u8,
  nop: Io<lio::api::ops::Nop>,
}

impl TwoNops {
  fn new() -> Self {
    Self { left: 2, nop: api::nop() }
  }
}

impl OpModel for TwoNops {
  type Item = std::io::Result<u8>;

  fn poll(&mut self, cx: &mut Cx<'_>) -> Step<Self::Item> {
    loop {
      if let Err(err) = ready!(self.nop.poll(cx)) {
        return Step::Done(Err(err));
      }
      self.left -= 1;
      if self.left == 0 {
        return Step::Done(Ok(2));
      }
      self.nop = api::nop();
    }
  }
}

#[test]
fn io_runs_a_model_of_your_own_like_one_of_lio_s() {
  let (backend, lio) = scripted(1);

  // With a callback.
  let done = Rc::new(RefCell::new(None));
  let sink = Rc::clone(&done);
  api::run(TwoNops::new())
    .with_lio(&lio)
    .when_done(move |item| *sink.borrow_mut() = Some(item));
  finish_only(&backend, &lio, 0);
  finish_only(&backend, &lio, 0);
  assert_eq!(done.borrow_mut().take().unwrap().unwrap(), 2);

  // As a future.
  let mut future = Box::pin(std::future::IntoFuture::into_future(
    api::run(TwoNops::new()).with_lio(&lio),
  ));
  let mut cx = Context::from_waker(Waker::noop());
  assert!(future.as_mut().poll(&mut cx).is_pending());
  finish_only(&backend, &lio, 0);
  assert!(future.as_mut().poll(&mut cx).is_pending());
  finish_only(&backend, &lio, 0);
  let Poll::Ready(item) = future.as_mut().poll(&mut cx) else {
    panic!("the model is done");
  };
  assert_eq!(item.unwrap(), 2);

  // One slot was enough throughout: each nop used the model's own.
  assert_eq!(backend.pushed().len(), 4);
  assert!(!lio.is_full());
}

#[test]
fn a_model_doing_two_things_at_once_still_fits_one_slot() {
  let (backend, lio) = scripted(1);
  let stdin = Resource::stdin();
  let join = Join::new(
    api::read(&stdin, Vec::<u8>::with_capacity(8)),
    api::read(&stdin, Vec::<u8>::with_capacity(8)),
  );
  let (items, _task) = collect(&lio, join);

  // The second read has no slot of its own to go to. It runs in the
  // model's slot once the first is out of it, not after the model is done.
  finish_only(&backend, &lio, 3);
  assert!(items.borrow().is_empty());
  finish_only(&backend, &lio, 2);

  let ((first, _), (second, _)) = items.borrow_mut().pop().unwrap();
  assert_eq!((first.unwrap(), second.unwrap()), (3, 2));
  assert_eq!(backend.pushed().len(), 2);
  assert!(!lio.is_full());
}

#[test]
fn a_model_takes_one_slot_however_many_steps_it_has() {
  let (backend, lio) = scripted(2);
  let (items, _task) = collect(&lio, TwoNops::new());
  // The second slot stays free for somebody else the whole time.
  assert!(!lio.is_full());
  finish_only(&backend, &lio, 0);
  assert!(!lio.is_full());
  finish_only(&backend, &lio, 0);
  assert_eq!(items.borrow_mut().pop().unwrap().unwrap(), 2);
  let ids = backend.pushed();
  assert_eq!(ids[0], ids[1], "both steps ran in the model's slot");
}

#[test]
fn run_stream_gives_a_stream_s_items_one_at_a_time() {
  let (backend, lio) = scripted(4);
  let from = Resource::stdin();
  let read = api::read(&from, Vec::<u8>::with_capacity(4));
  let mut stream =
    api::run_stream(Chunks { from, read, ended: false }).with_lio(&lio).start();
  let mut cx = Context::from_waker(Waker::noop());

  let mut lens = Vec::new();
  for result in [4, 2, 0] {
    assert!(Box::pin(stream.next()).as_mut().poll(&mut cx).is_pending());
    finish_only(&backend, &lio, result);
    let Poll::Ready(Some((_, buf))) =
      Box::pin(stream.next()).as_mut().poll(&mut cx)
    else {
      panic!("the chunk that was read is an item");
    };
    lens.push(buf.len());
  }
  assert_eq!(lens, [4, 2, 0]);
  // The model is done: so is the stream.
  let end = Box::pin(stream.next()).as_mut().poll(&mut cx);
  assert!(matches!(end, Poll::Ready(None)));
  assert!(!lio.is_full());
}
