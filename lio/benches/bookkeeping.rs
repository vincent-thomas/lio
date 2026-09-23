use std::cell::Cell;
use std::future::Future;
use std::hint::black_box;
use std::io;
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
use lio::backend::op::Op;
use lio::backend::{IoBackend, OpCompleted};
use lio::time::Clock;
use lio::{Lio, api};

const QUEUE_DEPTHS: [usize; 4] = [1, 32, 256, 1024];

/// A zero-syscall backend that makes every queued NOP ready on `flush`.
///
/// Its direct benchmark is the baseline for the driver benchmark: both execute
/// the same backend work, so the difference is lio's scheduling, registration,
/// completion, callback, and removal bookkeeping.
#[derive(Default)]
struct ImmediateBackend {
  queued: Vec<u64>,
  ready: Vec<u64>,
}

impl IoBackend for ImmediateBackend {
  fn init(&mut self, cap: usize) -> io::Result<()> {
    self.queued.reserve(cap);
    self.ready.reserve(cap);
    Ok(())
  }

  fn push(&mut self, id: u64, op: Op, _step_bump: &mut Bump) {
    assert!(matches!(op, Op::Nop), "benchmark backend only supports NOP");
    self.queued.push(id);
  }

  fn flush(&mut self) -> io::Result<()> {
    self.ready.append(&mut self.queued);
    Ok(())
  }

  fn wait(
    &mut self,
    _timeout: Option<Duration>,
    completed: &mut Vec<OpCompleted>,
  ) -> io::Result<()> {
    completed.clear();
    completed.extend(self.ready.drain(..).map(|id| OpCompleted::new(id, 0)));
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

  // SAFETY: the only submitted action is Nop, which uses no pointers, owned
  // handles, or output metadata; ImmediateBackend completes each queued Nop once.
  unsafe fn complete(&mut self, _: Completion) -> OpResult<()> {
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
    .measurement_time(Duration::from_millis(1500))
    .sample_size(30);
  targets = direct_backend, driver_callback, driver_channel, stream_executor, timers
}
criterion_main!(benches);
