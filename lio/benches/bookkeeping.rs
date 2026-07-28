use std::cell::Cell;
use std::hint::black_box;
use std::io;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::Duration;

use bumpalo::Bump;
use criterion::{
  BenchmarkId, Criterion, Throughput, criterion_group, criterion_main,
};
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
  targets = direct_backend, driver_callback, driver_channel, timers
}
criterion_main!(benches);
