//! The poll-shaped `lio::model` prototype: leaves, composites, parallel I/O
//! and cancellation, without real I/O where the order of completions matters.

use std::cell::RefCell;
use std::ffi::CString;
use std::future::Future;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

use lio::api::resource::Resource;
use lio::api::{self, OpenFlags};
use lio::backend::ds::{DSBackend, DSConfig};
use lio::backend::op::Op;
use lio::backend::testing::{Checked, POISON, ScriptedBackend};
use lio::model::{self, Cx, Join, Model, Read, Step, Task, Write};
use lio::{BufResult, Lio};

fn scripted(cap: usize) -> (ScriptedBackend, Lio) {
  let backend = ScriptedBackend::new();
  let lio = Lio::new_with_backend(Checked::new(backend.clone()), cap).unwrap();
  (backend, lio)
}

/// Spawns `model` and collects every item it produces.
fn collect<M>(lio: &Lio, model: M) -> (Rc<RefCell<Vec<M::Item>>>, Task)
where
  M: Model + 'static,
{
  let items = Rc::new(RefCell::new(Vec::new()));
  let sink = Rc::clone(&items);
  let task = model::spawn(lio, model, move |item| sink.borrow_mut().push(item));
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
    collect(&lio, Read::new(&stdin, Vec::<u8>::with_capacity(8)));

  assert!(items.borrow().is_empty());
  finish_only(&backend, &lio, 3);

  let mut items = items.borrow_mut();
  let (result, buf) = items.pop().unwrap();
  assert_eq!(result.unwrap(), 3);
  assert_eq!(buf, vec![POISON; 3]);
  assert!(items.is_empty());
}

#[test]
fn copy_alternates_reads_and_writes_until_the_source_ends() {
  let (backend, lio) = scripted(4);
  let (stdin, stdout) = (Resource::stdin(), Resource::stdout());
  let (items, _task) =
    collect(&lio, lio::io::copy(&stdin, &stdout).into_inner());

  assert!(matches!(only_op(&backend, &lio), Op::Read { .. }));
  finish_only(&backend, &lio, 5);
  // A short write: the rest of the buffer is written again.
  assert!(matches!(only_op(&backend, &lio), Op::Write { .. }));
  finish_only(&backend, &lio, 2);
  assert!(matches!(only_op(&backend, &lio), Op::Write { .. }));
  finish_only(&backend, &lio, 3);
  assert!(matches!(only_op(&backend, &lio), Op::Read { .. }));
  assert!(items.borrow().is_empty());
  finish_only(&backend, &lio, 0);

  assert_eq!(items.borrow_mut().pop().unwrap().unwrap(), 5);
  assert!(backend.in_flight().is_empty());
}

#[test]
fn copy_stops_at_the_first_error() {
  let (backend, lio) = scripted(4);
  let (stdin, stdout) = (Resource::stdin(), Resource::stdout());
  let (items, _task) =
    collect(&lio, lio::io::copy(&stdin, &stdout).into_inner());

  finish_only(&backend, &lio, 4);
  finish_only(&backend, &lio, -9);

  let err = items.borrow_mut().pop().unwrap().unwrap_err();
  assert_eq!(err.raw_os_error(), Some(9));
  assert!(backend.in_flight().is_empty());
}

#[test]
fn join_has_both_operations_in_flight_and_waits_for_both() {
  let (backend, lio) = scripted(4);
  let stdin = Resource::stdin();
  let join = Join::new(
    Read::new(&stdin, Vec::<u8>::with_capacity(8)),
    Read::new(&stdin, Vec::<u8>::with_capacity(8)),
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
    lio::io::copy(&stdin, &stdout).into_inner(),
    Write::new(&stdout, b"hi".to_vec()),
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
    Read::new(&stdin, Vec::<u8>::with_capacity(8)),
    Read::new(&stdin, Vec::<u8>::with_capacity(8)),
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
  let mut future =
    Box::pin(model::run(&lio, Read::new(&stdin, Vec::<u8>::with_capacity(8))));
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
  let mut future =
    Box::pin(model::run(&lio, Read::new(&stdin, Vec::<u8>::with_capacity(8))));
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
  read: Read<Vec<u8>>,
}

impl Model for Chunks {
  type Item = BufResult<usize, Vec<u8>>;

  fn poll(&mut self, cx: &mut Cx<'_>) -> Step<Self::Item> {
    let (result, buf) = lio::ready!(self.read.poll(cx));
    match result {
      Ok(n) if n > 0 => {
        self.read = Read::new(&self.from, Vec::<u8>::with_capacity(4));
        Step::Yield((Ok(n), buf))
      }
      other => Step::Done((other, buf)),
    }
  }
}

#[test]
fn a_stream_yields_until_it_is_done() {
  let (backend, lio) = scripted(4);
  let from = Resource::stdin();
  let read = Read::new(&from, Vec::<u8>::with_capacity(4));
  let (items, _task) = collect(&lio, Chunks { from, read });

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
  loop {
    if let Some(resource) = opened.try_recv() {
      return resource.unwrap();
    }
    lio.try_run().unwrap();
  }
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
  write: Write<Vec<u8>>,
}

impl WriteAll {
  fn new(to: &Resource, data: Vec<u8>) -> Self {
    Self { to: to.clone(), offset: 0, write: Write::at(to, data, 0) }
  }
}

impl Model for WriteAll {
  type Item = std::io::Result<u64>;

  fn poll(&mut self, cx: &mut Cx<'_>) -> Step<Self::Item> {
    loop {
      let (result, mut buf) = lio::ready!(self.write.poll(cx));
      let written = match result {
        Ok(written) => written,
        Err(err) => return Step::Done(Err(err)),
      };
      self.offset += written as u64;
      buf.drain(..written);
      if buf.is_empty() {
        return Step::Done(Ok(self.offset));
      }
      self.write = Write::at(&self.to, buf, self.offset);
    }
  }
}

/// Reads a resource from the start until it ends.
struct ReadToEnd {
  from: Resource,
  bytes: Vec<u8>,
  read: Read<Vec<u8>>,
}

impl ReadToEnd {
  fn new(from: &Resource) -> Self {
    let read = Read::at(from, Vec::with_capacity(512), 0);
    Self { from: from.clone(), bytes: Vec::new(), read }
  }
}

impl Model for ReadToEnd {
  type Item = std::io::Result<Vec<u8>>;

  fn poll(&mut self, cx: &mut Cx<'_>) -> Step<Self::Item> {
    loop {
      let (result, mut buf) = lio::ready!(self.read.poll(cx));
      match result {
        Ok(0) => return Step::Done(Ok(std::mem::take(&mut self.bytes))),
        Ok(_) => self.bytes.extend_from_slice(&buf),
        Err(err) => return Step::Done(Err(err)),
      }
      buf.clear();
      self.read = Read::at(&self.from, buf, self.bytes.len() as u64);
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
  let (copied, _task) = collect(&lio, lio::io::copy(&src, &dst).into_inner());
  assert_eq!(run_to_item(&lio, &copied).unwrap(), data.len() as u64);

  let dst = open(&lio, "/tmp/lio_model_copy_dst", OpenFlags::READ);
  let (read_back, _task) = collect(&lio, ReadToEnd::new(&dst));
  assert_eq!(run_to_item(&lio, &read_back).unwrap(), data);
}
