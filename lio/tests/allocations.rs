//! A driver sized for its load allocates nothing per operation: awaiting,
//! streaming and callbacks reuse the memory reserved up front.
//!
//! A counting global allocator measures a window after a warm-up round, so
//! arenas and queues that grow to their working size on first use are not
//! counted.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::future::{Future, IntoFuture};
use std::io;
use std::pin::pin;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use bumpalo::Bump;
use lio::api::IoStream;
use lio::api::op::{Action, Completion, OpModel, OpResult, StreamOpModel};
use lio::backend::op::Op;
use lio::backend::{IoBackend, OpCompleted};
use lio::{Lio, api};

/// Counts allocations while `COUNTING` is set.
struct Counting;

static COUNTING: AtomicBool = AtomicBool::new(false);
static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

// SAFETY: forwards to the system allocator unchanged.
unsafe impl GlobalAlloc for Counting {
  unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
    if COUNTING.load(Ordering::Relaxed) {
      ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
    }
    // SAFETY: forwarded from the caller.
    unsafe { System.alloc(layout) }
  }

  unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
    // SAFETY: forwarded from the caller.
    unsafe { System.dealloc(ptr, layout) }
  }

  unsafe fn realloc(
    &self,
    ptr: *mut u8,
    layout: Layout,
    new_size: usize,
  ) -> *mut u8 {
    if COUNTING.load(Ordering::Relaxed) {
      ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
    }
    // SAFETY: forwarded from the caller.
    unsafe { System.realloc(ptr, layout, new_size) }
  }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// Allocations made by `f`.
fn allocations(f: impl FnOnce()) -> usize {
  ALLOCATIONS.store(0, Ordering::SeqCst);
  COUNTING.store(true, Ordering::SeqCst);
  f();
  COUNTING.store(false, Ordering::SeqCst);
  ALLOCATIONS.load(Ordering::SeqCst)
}

/// Completes every operation with 0 on the next `wait`, from storage
/// reserved by `init`.
#[derive(Default)]
struct Immediate {
  pushed: Vec<u64>,
}

impl IoBackend for Immediate {
  fn init(&mut self, cap: usize) -> io::Result<()> {
    self.pushed.reserve(cap);
    Ok(())
  }

  fn push(&mut self, id: u64, _: Op, _: &mut Bump) {
    self.pushed.push(id);
  }

  fn flush(&mut self) -> io::Result<()> {
    Ok(())
  }

  fn wait(
    &mut self,
    _: Option<Duration>,
    completed: &mut Vec<OpCompleted>,
  ) -> io::Result<()> {
    completed.clear();
    completed.extend(self.pushed.drain(..).map(|id| OpCompleted::new(id, 0)));
    Ok(())
  }
}

/// Yields `left` items, one per completion, then finishes.
struct Countdown {
  left: u32,
}

impl OpModel for Countdown {
  type Item = u32;

  fn action(&mut self) -> Action {
    Action::Io(Op::Nop)
  }

  fn complete(&mut self, _: Completion) -> OpResult<u32> {
    self.left -= 1;
    if self.left == 0 { OpResult::Done(0) } else { OpResult::Yield(self.left) }
  }
}

impl StreamOpModel for Countdown {}

const CAP: usize = 4;

/// Awaits `CAP` nops at once, driving the driver by hand.
fn await_nops(lio: &Lio) {
  let mut cx = Context::from_waker(Waker::noop());
  let mut futures: [_; CAP] =
    std::array::from_fn(|_| Some(api::nop().with_lio(lio).into_future()));
  loop {
    let mut pending = false;
    for slot in &mut futures {
      if let Some(future) = slot {
        match pin!(future).poll(&mut cx) {
          Poll::Ready(result) => {
            result.unwrap();
            *slot = None;
          }
          Poll::Pending => pending = true,
        }
      }
    }
    if !pending {
      return;
    }
    lio.try_run().unwrap();
  }
}

/// Reads a whole stream of `items` items, driving the driver by hand.
fn stream_items(lio: &Lio, items: u32) {
  let mut cx = Context::from_waker(Waker::noop());
  let mut stream = IoStream::from_op(Countdown { left: items }).with_lio(lio);
  let mut seen = 0;
  loop {
    match pin!(stream.next()).poll(&mut cx) {
      Poll::Ready(Some(_)) => seen += 1,
      Poll::Ready(None) => break,
      Poll::Pending => {
        lio.try_run().unwrap();
      }
    }
  }
  assert_eq!(seen, items);
}

/// Runs `CAP` callbacks to completion.
fn callbacks(lio: &Lio, done: &Rc<Cell<usize>>) {
  for _ in 0..CAP {
    let done = done.clone();
    api::nop().with_lio(lio).when_done(move |result| {
      result.unwrap();
      done.set(done.get() + 1);
    });
  }
  lio.try_run().unwrap();
  lio.try_run().unwrap();
}

#[test]
fn awaiting_allocates_nothing_per_operation() {
  let lio = Lio::new_with_backend(Immediate::default(), CAP).unwrap();
  await_nops(&lio);
  assert_eq!(allocations(|| (0..8).for_each(|_| await_nops(&lio))), 0);
}

#[test]
fn a_stream_read_as_it_yields_allocates_nothing() {
  let lio = Lio::new_with_backend(Immediate::default(), CAP).unwrap();
  stream_items(&lio, 5);
  assert_eq!(allocations(|| stream_items(&lio, 32)), 0);
}

#[test]
fn callbacks_allocate_nothing_per_operation() {
  let lio = Lio::new_with_backend(Immediate::default(), CAP).unwrap();
  let done = Rc::new(Cell::new(0));
  callbacks(&lio, &done);
  assert_eq!(allocations(|| (0..8).for_each(|_| callbacks(&lio, &done))), 0);
  assert_eq!(done.get(), CAP * 9);
}
