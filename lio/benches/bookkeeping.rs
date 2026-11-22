//! What lio's bookkeeping costs, with no system calls.
//!
//! Every group except `timer` runs on [`BenchBackend`]. A flush completes
//! queued nops and reads in memory. `cancel` keeps flushed operations in
//! flight until the driver cancels them, then completes those in memory too.
//! `timer` drives [`Clock`](lio::time::Clock) directly. None of this enters
//! the kernel. Kernel time is `cargo bench -p lio --bench backends`.
//!
//! Throughput is operations per second. `resubmit` counts yielded items.
//!
//! - `bookkeeping/no_driver` — push, flush, and wait `qd` nops on the backend
//!   alone. The driver groups sit on top of this cost.
//! - `bookkeeping/round_trip` — submit `qd` nops and deliver every completion
//!   through `when_done`, in one `try_run`. The gap above `no_driver` is
//!   registration, routing, the callback, and releasing the slot.
//! - `bookkeeping/cancel` — submit `qd` nops, flush them so they stay in
//!   flight, drop the futures, then route the cancellation completions.
//! - `bookkeeping/buffer_256` — the round trip for `qd` reads of a 256-byte
//!   buffer. The backend does not write the buffer, so the gap above
//!   `round_trip` is building the iovec and handing the buffer back.
//! - `bookkeeping/resubmit` — one yielded stream item with `steps` extra nops
//!   before it. A step rearms the same registration instead of releasing it.
//!   Throughput is items per second. The other groups count every operation.
//! - `bookkeeping/timer` — insert and fire `qd` timers on the wheel, advanced
//!   one tick with no driver. A sleep through `Lio` waits out a real 1 ms
//!   tick, which would hide the wheel.
//!
//! `qd` is 1, 32, 256, and 1024. This file is
//! `cargo bench -p lio --bench bookkeeping`. `mise run bench` runs it and
//! then the backend bench. Criterion flags go after `--`.

use std::cell::{Cell, RefCell};
use std::future::Future;
use std::hint::black_box;
use std::io;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::{
  Arc,
  atomic::{AtomicBool, Ordering},
};
use std::task::{Context, Poll, Wake, Waker};
use std::time::Duration;

use bumpalo::Bump;
use criterion::{
  BenchmarkId, Criterion, Throughput, criterion_group, criterion_main,
};
use lio::api::op::{Action, Completion, OpModel, OpResult, StreamOpModel};
use lio::api::resource::Resource;
use lio::backend::op::Op;
use lio::backend::{IoBackend, OpCompleted};
use lio::time::Clock;
use lio::{Lio, api};

const QUEUE_DEPTHS: [usize; 4] = [1, 32, 256, 1024];

/// Reads in `bookkeeping/buffer_256`. The backend reports this many bytes
/// and does not touch the buffer.
const BUFFER_LEN: usize = 256;

/// A negative completion is an error to the op model. Not an errno.
const CANCEL_RESULT: isize = -1;

const SLOT_EMPTY: u8 = 0;
const SLOT_QUEUED: u8 = 1;
const SLOT_IN_FLIGHT: u8 = 2;

/// In-memory backend for nops and fixed-size reads.
///
/// `flush` completes everything queued, unless constructed with
/// [`BenchBackend::holding`]: flushed ops then stay in flight until `cancel`.
/// `wait` never blocks. Cancel of an in-flight op is indexed by slab slot, so
/// a batch cancel is not a scan.
#[derive(Default)]
struct BenchBackend {
  queued: Vec<(u64, isize)>,
  ready: Vec<(u64, isize)>,
  hold: bool,
  slots: Vec<u8>,
  slot_id: Vec<u64>,
}

impl BenchBackend {
  /// Flushed ops stay in flight until `cancel` completes them.
  fn holding() -> Self {
    Self { hold: true, ..Self::default() }
  }
}

impl IoBackend for BenchBackend {
  fn init(&mut self, cap: usize) -> io::Result<()> {
    self.queued.reserve(cap);
    self.ready.reserve(cap);
    if self.hold {
      self.slots.resize(cap, SLOT_EMPTY);
      self.slot_id.resize(cap, 0);
    }
    Ok(())
  }

  fn push(&mut self, id: u64, op: Op, _step_bump: &mut Bump) {
    let result = match op {
      Op::Nop => 0,
      Op::Read { .. } => BUFFER_LEN as isize,
      _ => panic!("bench backend only supports nop and read"),
    };
    if self.hold {
      let slot = id as u32 as usize;
      assert!(
        slot < self.slots.len(),
        "bench backend slot {slot} is past its capacity"
      );
      assert_eq!(
        self.slots[slot], SLOT_EMPTY,
        "bench backend slot {slot} is still in use"
      );
      self.slots[slot] = SLOT_QUEUED;
      self.slot_id[slot] = id;
    } else {
      self.queued.push((id, result));
    }
  }

  fn flush(&mut self) -> io::Result<()> {
    if self.hold {
      for state in &mut self.slots {
        if *state == SLOT_QUEUED {
          *state = SLOT_IN_FLIGHT;
        }
      }
    } else {
      self.ready.append(&mut self.queued);
    }
    Ok(())
  }

  fn cancel(&mut self, id: u64) {
    if self.hold {
      let slot = id as u32 as usize;
      if slot < self.slots.len()
        && self.slot_id[slot] == id
        && (self.slots[slot] == SLOT_QUEUED
          || self.slots[slot] == SLOT_IN_FLIGHT)
      {
        self.slots[slot] = SLOT_EMPTY;
        self.ready.push((id, CANCEL_RESULT));
      }
      return;
    }
    if let Some(index) =
      self.queued.iter().position(|(queued, _)| *queued == id)
    {
      self.queued.swap_remove(index);
      self.ready.push((id, CANCEL_RESULT));
    }
  }

  fn wait(
    &mut self,
    _timeout: Option<Duration>,
    completed: &mut Vec<OpCompleted>,
  ) -> io::Result<()> {
    completed.clear();
    completed.extend(
      self.ready.drain(..).map(|(id, result)| OpCompleted::new(id, result)),
    );
    Ok(())
  }
}

fn no_driver(criterion: &mut Criterion) {
  let mut group = criterion.benchmark_group("bookkeeping/no_driver");
  for depth in QUEUE_DEPTHS {
    let mut backend = BenchBackend::default();
    backend.init(depth).unwrap();
    let mut bump = Bump::new();
    let mut completed = Vec::with_capacity(depth);

    group.throughput(Throughput::Elements(depth as u64));
    group.bench_with_input(
      BenchmarkId::from_parameter(format!("qd_{depth}")),
      &depth,
      |bencher, &depth| {
        bencher.iter(|| {
          for id in 0..depth as u64 {
            backend.push(id, Op::Nop, &mut bump);
          }
          backend.flush().unwrap();
          backend.wait(Some(Duration::ZERO), &mut completed).unwrap();
          assert_eq!(completed.len(), depth);
          black_box(&completed);
        });
      },
    );
  }
  group.finish();
}

fn round_trip(criterion: &mut Criterion) {
  let mut group = criterion.benchmark_group("bookkeeping/round_trip");
  for depth in QUEUE_DEPTHS {
    let lio = Lio::new_with_backend(BenchBackend::default(), depth).unwrap();
    let completed = Rc::new(Cell::new(0usize));

    group.throughput(Throughput::Elements(depth as u64));
    group.bench_with_input(
      BenchmarkId::from_parameter(format!("qd_{depth}")),
      &depth,
      |bencher, &depth| {
        bencher.iter(|| {
          let before = completed.get();
          for _ in 0..depth {
            let completed = Rc::clone(&completed);
            api::nop().with_lio(&lio).when_done(move |result| {
              result.unwrap();
              completed.set(completed.get() + 1);
            });
          }
          assert_eq!(lio.try_run().unwrap(), depth);
          assert_eq!(completed.get() - before, depth);
        });
      },
    );
  }
  group.finish();
}

fn cancel(criterion: &mut Criterion) {
  let mut group = criterion.benchmark_group("bookkeeping/cancel");
  let ready = Arc::new(NopWaker::default());
  let waker = Waker::from(Arc::clone(&ready));
  let mut cx = Context::from_waker(&waker);
  for depth in QUEUE_DEPTHS {
    let lio = Lio::new_with_backend(BenchBackend::holding(), depth).unwrap();

    group.throughput(Throughput::Elements(depth as u64));
    group.bench_with_input(
      BenchmarkId::from_parameter(format!("qd_{depth}")),
      &depth,
      |bencher, &depth| {
        bencher.iter(|| {
          let mut futures = Vec::with_capacity(depth);
          for _ in 0..depth {
            let mut fut = api::nop().with_lio(&lio).into_future();
            assert!(Pin::new(&mut fut).poll(&mut cx).is_pending());
            futures.push(fut);
          }
          assert_eq!(lio.try_run().unwrap(), 0);
          drop(futures);
          assert_eq!(lio.try_run().unwrap(), depth);
        });
      },
    );
  }
  group.finish();
}

fn buffer(criterion: &mut Criterion) {
  let mut group =
    criterion.benchmark_group(format!("bookkeeping/buffer_{BUFFER_LEN}"));
  // Borrowed, so the op's descriptor is not closed, and the backend never
  // reads it.
  let file = Resource::stdout();
  for depth in QUEUE_DEPTHS {
    let lio = Lio::new_with_backend(BenchBackend::default(), depth).unwrap();
    let buffers = Rc::new(RefCell::new(
      (0..depth).map(|_| vec![0u8; BUFFER_LEN]).collect::<Vec<_>>(),
    ));
    let completed = Rc::new(Cell::new(0usize));

    group.throughput(Throughput::Elements(depth as u64));
    group.bench_with_input(
      BenchmarkId::from_parameter(format!("qd_{depth}")),
      &depth,
      |bencher, &depth| {
        bencher.iter(|| {
          let before = completed.get();
          for _ in 0..depth {
            let buf = buffers.borrow_mut().pop().unwrap();
            let buffers = Rc::clone(&buffers);
            let completed = Rc::clone(&completed);
            api::read_at(&file, buf, 0).with_lio(&lio).when_done(
              move |(result, buf)| {
                assert_eq!(result.unwrap(), BUFFER_LEN);
                black_box(buf[0]);
                buffers.borrow_mut().push(buf);
                completed.set(completed.get() + 1);
              },
            );
          }
          assert_eq!(lio.try_run().unwrap(), depth);
          assert_eq!(completed.get() - before, depth);
        });
      },
    );
  }
  group.finish();
}

/// One yielded item, with `steps` nops completed before it.
struct SteppedNop {
  steps: usize,
  left: usize,
}

impl OpModel for SteppedNop {
  type Item = ();

  fn action(&mut self) -> Action {
    Action::Io(Op::Nop)
  }

  fn complete(&mut self, _: Completion) -> OpResult<()> {
    if self.left > 0 {
      self.left -= 1;
      OpResult::Again
    } else {
      self.left = self.steps;
      OpResult::Yield(())
    }
  }
}

impl StreamOpModel for SteppedNop {}

#[derive(Default)]
struct NopWaker(AtomicBool);

impl Wake for NopWaker {
  fn wake(self: Arc<Self>) {
    self.wake_by_ref();
  }

  fn wake_by_ref(self: &Arc<Self>) {
    self.0.store(true, Ordering::Relaxed);
  }
}

fn resubmit(criterion: &mut Criterion) {
  let mut group = criterion.benchmark_group("bookkeeping/resubmit");
  for steps in [0usize, 1, 8] {
    let lio = Lio::new_with_backend(BenchBackend::default(), 1).unwrap();
    let mut stream =
      api::io::IoStream::from_op(SteppedNop { steps, left: steps })
        .with_lio(&lio);
    let ready = Arc::new(NopWaker::default());
    let waker = Waker::from(Arc::clone(&ready));
    let mut cx = Context::from_waker(&waker);

    group.throughput(Throughput::Elements(1));
    group.bench_function(BenchmarkId::new("steps", steps), |bencher| {
      bencher.iter(|| {
        let mut next = std::pin::pin!(stream.next());
        assert!(next.as_mut().poll(&mut cx).is_pending());
        loop {
          assert_eq!(lio.try_run().unwrap(), 1);
          if ready.0.swap(false, Ordering::Relaxed)
            && let Poll::Ready(item) = next.as_mut().poll(&mut cx)
          {
            assert_eq!(item, Some(()));
            break;
          }
        }
      });
    });
  }
  group.finish();
}

fn timer(criterion: &mut Criterion) {
  let mut group = criterion.benchmark_group("bookkeeping/timer");
  for depth in QUEUE_DEPTHS {
    let mut clock = Clock::with_capacity(depth);

    group.throughput(Throughput::Elements(depth as u64));
    group.bench_with_input(
      BenchmarkId::from_parameter(format!("qd_{depth}")),
      &depth,
      |bencher, &depth| {
        bencher.iter(|| {
          for id in 0..depth as u64 {
            clock.schedule(id, Duration::ZERO);
          }
          clock.advance_by(1);
          assert_eq!(clock.poll_expired().count(), depth);
        });
      },
    );
  }
  group.finish();
}

criterion_group! {
  name = benches;
  config = Criterion::default()
    .warm_up_time(Duration::from_millis(500))
    .measurement_time(Duration::from_secs(2))
    .sample_size(50);
  targets = no_driver, round_trip, cancel, buffer, resubmit, timer
}
criterion_main!(benches);
