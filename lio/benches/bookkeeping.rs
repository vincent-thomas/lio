//! Bookkeeping benchmarks.
//!
//! Each group times lio's scheduling, submission, completion routing,
//! cancellation, or buffer handling. [`ImmediateBackend`] (in this file)
//! completes nops in memory. `read_buffer` uses
//! [`ScriptedBackend`](lio::backend::testing::ScriptedBackend), which writes
//! the read buffer on completion the way a kernel would. Neither backend
//! makes a system call, so a result here is the runtime's own work. Kernel
//! time is what `cargo bench -p lio --bench backends` measures; those numbers
//! include it.
//!
//! - `bookkeeping/direct_backend` — push, flush, and wait `qd` nops on the
//!   backend with no driver. Baseline for the driver groups.
//! - `bookkeeping/driver_callback` — submit `qd` nops and deliver completions
//!   through `when_done`.
//! - `bookkeeping/driver_channel` — the same round trip, onto a channel.
//! - `bookkeeping/stream_executor` — one stream item with `again` extra nops
//!   between yields.
//! - `bookkeeping/timer_wheel` — schedule and expire `qd` zero-duration
//!   timers. No backend.
//! - `bookkeeping/cancel` — submit `qd` nops, flush them so they stay in
//!   flight, drop the futures, then route the cancellation completions.
//! - `bookkeeping/read_buffer` — submit `qd` 4 KiB reads and take the buffers
//!   back. The scripted backend fills each buffer; the descriptor is not read.
//!
//! `qd` is the batch size. Throughput is operations per second. This file is
//! `cargo bench -p lio --bench bookkeeping`. `mise run bench` runs it and
//! then the backend bench. Criterion flags (`--sample-size`,
//! `--measurement-time`) go after `--`.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::future::Future;
use std::hint::black_box;
use std::io;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::mpsc;
use std::sync::{
  Arc,
  atomic::{AtomicBool, Ordering},
};
use std::task::{Context, Wake, Waker};
use std::time::Duration;

use bumpalo::Bump;
use criterion::{
  BenchmarkId, Criterion, Throughput, criterion_group, criterion_main,
};
use lio::api::op::{Action, Completion, OpModel, OpResult, StreamOpModel};
use lio::api::resource::Resource;
use lio::backend::op::Op;
use lio::backend::testing::ScriptedBackend;
use lio::backend::{IoBackend, OpCompleted};
use lio::time::Clock;
use lio::{Lio, api};

const QUEUE_DEPTHS: [usize; 4] = [1, 32, 256, 1024];

/// Any negative completion is an error to the op model. This is not an errno.
const CANCEL_RESULT: isize = -1;

/// A zero-syscall backend for nops.
///
/// By default `flush` makes every queued nop ready, so the direct benchmark
/// and the driver benchmark do the same backend work. The difference is lio's
/// scheduling, registration, completion, callback, and removal.
///
/// [`ImmediateBackend::holding`] keeps flushed ops in flight until `cancel`
/// queues a completion. `wait` never blocks.
#[derive(Default)]
struct ImmediateBackend {
  queued: Vec<u64>,
  in_flight: HashSet<u64>,
  ready: Vec<(u64, isize)>,
  hold: bool,
}

impl ImmediateBackend {
  fn holding() -> Self {
    Self { hold: true, ..Self::default() }
  }
}

impl IoBackend for ImmediateBackend {
  fn init(&mut self, cap: usize) -> io::Result<()> {
    self.queued.reserve(cap);
    self.in_flight.reserve(cap);
    self.ready.reserve(cap);
    Ok(())
  }

  fn push(&mut self, id: u64, op: Op, _step_bump: &mut Bump) {
    assert!(matches!(op, Op::Nop), "benchmark backend only supports NOP");
    self.queued.push(id);
  }

  fn flush(&mut self) -> io::Result<()> {
    if self.hold {
      self.in_flight.extend(self.queued.drain(..));
    } else {
      self.ready.extend(self.queued.drain(..).map(|id| (id, 0)));
    }
    Ok(())
  }

  fn cancel(&mut self, id: u64) {
    if let Some(index) = self.queued.iter().position(|queued| *queued == id) {
      self.queued.swap_remove(index);
      self.ready.push((id, CANCEL_RESULT));
      return;
    }
    if self.in_flight.remove(&id) {
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

fn direct_backend(criterion: &mut Criterion) {
  let mut group = criterion.benchmark_group("bookkeeping/direct_backend");
  for depth in QUEUE_DEPTHS {
    let mut backend = ImmediateBackend::default();
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

fn driver_callback(criterion: &mut Criterion) {
  let mut group = criterion.benchmark_group("bookkeeping/driver_callback");
  for depth in QUEUE_DEPTHS {
    let lio =
      Lio::new_with_backend(ImmediateBackend::default(), depth).unwrap();
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

fn driver_channel(criterion: &mut Criterion) {
  let mut group = criterion.benchmark_group("bookkeeping/driver_channel");
  for depth in [1, 256] {
    let lio =
      Lio::new_with_backend(ImmediateBackend::default(), depth).unwrap();
    let (sender, receiver) = mpsc::channel();

    group.throughput(Throughput::Elements(depth as u64));
    group.bench_with_input(
      BenchmarkId::from_parameter(format!("qd_{depth}")),
      &depth,
      |bencher, &depth| {
        bencher.iter(|| {
          for _ in 0..depth {
            api::nop().with_lio(&lio).send_with(sender.clone());
          }
          assert_eq!(lio.try_run().unwrap(), depth);
          for _ in 0..depth {
            receiver.recv().unwrap().unwrap();
          }
        });
      },
    );
  }
  group.finish();
}

/// A stream with internal I/O steps between items. The backend completes one
/// step per driver turn, allowing the executor to run between completions.
struct SteppedStream {
  again: usize,
  remaining: usize,
}

impl OpModel for SteppedStream {
  type Item = ();

  fn action(&mut self) -> Action {
    Action::Io(Op::Nop)
  }

  fn complete(&mut self, _: Completion) -> OpResult<()> {
    if self.remaining > 0 {
      self.remaining -= 1;
      OpResult::Again
    } else {
      self.remaining = self.again;
      OpResult::Yield(())
    }
  }
}

impl StreamOpModel for SteppedStream {}

#[derive(Default)]
struct ReadyTask(AtomicBool);

impl Wake for ReadyTask {
  fn wake(self: Arc<Self>) {
    self.wake_by_ref();
  }

  fn wake_by_ref(self: &Arc<Self>) {
    self.0.store(true, Ordering::Relaxed);
  }
}

fn stream_executor(criterion: &mut Criterion) {
  let mut group = criterion.benchmark_group("bookkeeping/stream_executor");
  group.warm_up_time(Duration::from_secs(1));
  group.measurement_time(Duration::from_secs(4));
  group.sample_size(60);
  for again in [0, 1, 8] {
    let lio = Lio::new_with_backend(ImmediateBackend::default(), 1).unwrap();
    let mut stream =
      api::io::IoStream::from_op(SteppedStream { again, remaining: again })
        .with_lio(&lio);
    let ready = Arc::new(ReadyTask::default());
    let waker = Waker::from(Arc::clone(&ready));
    let mut cx = Context::from_waker(&waker);

    group.throughput(Throughput::Elements(1));
    group.bench_function(BenchmarkId::new("again", again), |bencher| {
      bencher.iter(|| {
        let mut next = std::pin::pin!(stream.next());
        assert!(next.as_mut().poll(&mut cx).is_pending());
        let mut polls = 0usize;
        loop {
          assert_eq!(lio.try_run().unwrap(), 1);
          if ready.0.swap(false, Ordering::Relaxed) {
            polls += 1;
            if let std::task::Poll::Ready(item) = next.as_mut().poll(&mut cx) {
              assert_eq!(item, Some(()));
              break;
            }
          }
        }
        black_box(polls);
      });
    });
  }
  group.finish();
}

fn cancel(criterion: &mut Criterion) {
  let mut group = criterion.benchmark_group("bookkeeping/cancel");
  let ready = Arc::new(ReadyTask::default());
  let waker = Waker::from(Arc::clone(&ready));
  let mut cx = Context::from_waker(&waker);
  for depth in QUEUE_DEPTHS {
    let lio =
      Lio::new_with_backend(ImmediateBackend::holding(), depth).unwrap();

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
          // Flush into the backend. Nothing completes until cancel.
          assert_eq!(lio.try_run().unwrap(), 0);
          drop(futures);
          assert_eq!(lio.try_run().unwrap(), depth);
        });
      },
    );
  }
  group.finish();
}

fn read_buffer(criterion: &mut Criterion) {
  let mut group = criterion.benchmark_group("bookkeeping/read_buffer");
  // A 1024-deep batch fills 4 MiB. Give Criterion time to repeat it.
  group.measurement_time(Duration::from_secs(4));
  // Borrowed, so dropping the op does not close the process's stdout.
  // ScriptedBackend never reads the descriptor.
  let file = Resource::stdout();
  for depth in QUEUE_DEPTHS {
    let lio =
      Lio::new_with_backend(ScriptedBackend::auto(4096), depth).unwrap();
    let buffers = Rc::new(RefCell::new(
      (0..depth).map(|_| vec![0u8; 4096]).collect::<Vec<_>>(),
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
                assert_eq!(result.unwrap(), 4096);
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

fn timers(criterion: &mut Criterion) {
  let mut group = criterion.benchmark_group("bookkeeping/timer_wheel");
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
  targets = direct_backend, driver_callback, driver_channel, stream_executor, cancel, read_buffer, timers
}
criterion_main!(benches);
