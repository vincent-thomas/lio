//! What lio's bookkeeping costs, with no system calls.
//!
//! Every group except `timer` runs on [`BenchBackend`], which completes nops
//! and reads in memory as the driver's flush starts them. `cancel` keeps
//! started operations in flight until the driver cancels them, then
//! completes those in memory too.
//! `timer` drives [`Clock`](lio::time::Clock) directly. None of this enters
//! the kernel. Kernel time is `cargo bench -p lio --bench backends`.
//!
//! Throughput is operations per second. `resubmit` counts yielded items.
//!
//! - `bookkeeping/no_driver` — push, flush, and wait `qd` nops on the backend
//!   and its table ([`Tracked`]) alone. The driver groups sit on top of this
//!   cost.
//! - `bookkeeping/round_trip` — submit `qd` nops and deliver every completion
//!   through `when_done`, in one `try_run`. The gap above `no_driver` is
//!   registration, routing, the callback, and releasing the slot.
//! - `bookkeeping/cancel` — submit `qd` nops, flush them so they stay in
//!   flight, drop the futures, then route the cancellation completions.
//! - `bookkeeping/buffer_256` — the round trip for `qd` reads of a 256-byte
//!   buffer. The backend does not write the buffer, so the gap above
//!   `round_trip` is building the iovec and handing the buffer back.
//! - `bookkeeping/resubmit` — one yielded stream item from a model that
//!   completes `steps` extra nops before it. Every nop is its own operation,
//!   submitted when the model is polled after the one before it.
//!   Throughput is items per second. The other groups count every operation.
//! - `bookkeeping/timer` — insert and fire `qd` timers on the wheel, advanced
//!   one tick with no driver. A sleep through `Lio` waits out a real 1 ms
//!   tick, which would hide the wheel.
//!
//! `qd` is 1, 32, 256, and 1024. This file is
//! `cargo bench -p lio --bench bookkeeping`. `mise run bench` runs it and
//! then the backend bench. Criterion flags go after `--`.

use lio::ready;
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
use lio::api::resource::Resource;
use lio::backend::op::{BorrowedOp, Op};
use lio::backend::{Done, IoBackend, OpKey, PerOp, Queued, Started, Tracked};
use lio::model::{Cx, Next, OpModel, OpStream};
use lio::time::Clock;
use lio::{Lio, api};

const QUEUE_DEPTHS: [usize; 4] = [1, 32, 256, 1024];

/// Reads in `bookkeeping/buffer_256`. The backend reports this many bytes
/// and does not touch the buffer.
const BUFFER_LEN: usize = 256;

/// A negative completion is an error to the op model. Not an errno.
const CANCEL_RESULT: isize = -1;

/// In-memory backend for nops and fixed-size reads.
///
/// `start` completes the operation, unless constructed with
/// [`BenchBackend::holding`]: started ops then stay in flight until
/// `cancel`. `wait` never blocks. An op in flight is found by its key, so a
/// batch cancel is not a scan.
#[derive(Default)]
struct BenchBackend {
  hold: bool,
  held: PerOp<()>,
}

impl BenchBackend {
  /// Started ops stay in flight until `cancel` completes them.
  fn holding() -> Self {
    Self { hold: true, ..Self::default() }
  }
}

impl IoBackend for BenchBackend {
  fn init(&mut self, cap: usize) -> io::Result<()> {
    if self.hold {
      self.held = PerOp::new(cap);
    }
    Ok(())
  }

  fn start(
    &mut self,
    op: Queued<'_>,
    _step_bump: &mut Bump,
    done: &mut Done<'_>,
  ) -> Started {
    let result = match op.as_op() {
      Op::Nop => 0,
      Op::Read { .. } => BUFFER_LEN as isize,
      _ => panic!("bench backend only supports nop and read"),
    };
    let (started, key, _) = op.begin();
    if self.hold {
      self.held.insert(key, ());
    } else {
      // SAFETY: this backend never copies an address. Every buffer the
      // benches submit is initialized for the length a read reports.
      unsafe { done.complete(key, result) };
    }
    started
  }

  fn cancel(&mut self, key: OpKey, done: &mut Done<'_>) {
    if self.held.take(key).is_some() {
      // SAFETY: this backend never copies an address, and the result is
      // negative.
      unsafe { done.cancelled(key, CANCEL_RESULT) };
    }
  }

  fn wait(
    &mut self,
    _timeout: Option<Duration>,
    _done: &mut Done<'_>,
  ) -> io::Result<()> {
    Ok(())
  }

  fn resource_close(&self) -> lio::api::resource::ResourceClose {
    lio::api::resource::ResourceClose::native()
  }
}

fn no_driver(criterion: &mut Criterion) {
  let mut group = criterion.benchmark_group("bookkeeping/no_driver");
  for depth in QUEUE_DEPTHS {
    let mut backend = Tracked::new(BenchBackend::default());
    backend.init(depth).unwrap();
    let mut completed = Vec::with_capacity(depth);

    group.throughput(Throughput::Elements(depth as u64));
    group.bench_with_input(
      BenchmarkId::from_parameter(format!("qd_{depth}")),
      &depth,
      |bencher, &depth| {
        bencher.iter(|| {
          for id in 0..depth as u64 {
            // SAFETY: a no-op borrows nothing.
            unsafe { backend.push(id, BorrowedOp::nop()) };
          }
          backend.flush();
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
  nop: api::Io<api::ops::Nop>,
}

impl SteppedNop {
  fn new(steps: usize) -> Self {
    Self { steps, left: steps, nop: api::nop() }
  }
}

impl OpStream for SteppedNop {
  type Item = ();

  fn poll_next(&mut self, cx: &mut Cx<'_>) -> Next<()> {
    loop {
      let _ = ready!(self.nop.poll(cx));
      self.nop = api::nop();
      if self.left == 0 {
        self.left = self.steps;
        return Next::Item(());
      }
      self.left -= 1;
    }
  }
}

#[derive(Default)]
struct NopWaker(AtomicBool);

/// One operation awaited: polled, the driver run, polled again.
fn awaited(criterion: &mut Criterion) {
  let mut group = criterion.benchmark_group("bookkeeping/await");
  let ready = Arc::new(NopWaker::default());
  let waker = Waker::from(Arc::clone(&ready));
  let mut cx = Context::from_waker(&waker);
  for depth in QUEUE_DEPTHS {
    let lio = Lio::new_with_backend(BenchBackend::default(), depth).unwrap();
    let mut futures = Vec::with_capacity(depth);

    group.throughput(Throughput::Elements(depth as u64));
    group.bench_with_input(
      BenchmarkId::from_parameter(format!("qd_{depth}")),
      &depth,
      |bencher, &depth| {
        bencher.iter(|| {
          for _ in 0..depth {
            let mut fut = api::nop().with_lio(&lio).into_future();
            assert!(Pin::new(&mut fut).poll(&mut cx).is_pending());
            futures.push(fut);
          }
          assert_eq!(lio.try_run().unwrap(), depth);
          for mut fut in futures.drain(..) {
            let Poll::Ready(result) = Pin::new(&mut fut).poll(&mut cx) else {
              panic!("an operation the driver completed is not ready");
            };
            result.unwrap();
          }
        });
      },
    );
  }
  group.finish();
}

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
      api::io::IoStream::from_op(SteppedNop::new(steps)).with_lio(&lio).start();
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
  targets = no_driver, round_trip, awaited, cancel, buffer, resubmit, timer
}
criterion_main!(benches);
