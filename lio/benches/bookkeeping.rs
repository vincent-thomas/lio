mod support;

use std::cell::Cell;
use std::hint::black_box;
use std::io;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::Duration;

use bumpalo::Bump;
use lio::backend::op::Op;
use lio::backend::{IoBackend, OpCompleted};
use lio::time::Clock;
use lio::{Lio, api};
use support::Harness;

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

fn main() {
  let harness = Harness::from_args();
  direct_backend(&harness);
  driver_callback(&harness);
  driver_channel(&harness);
  timers(&harness);
}

fn direct_backend(harness: &Harness) {
  for depth in QUEUE_DEPTHS {
    let mut backend = ImmediateBackend::default();
    backend.init(depth).unwrap();
    let mut bump = Bump::new();
    let mut completed = Vec::with_capacity(depth);

    harness.bench(
      &format!("bookkeeping/direct_backend/qd_{depth}"),
      depth as u64,
      || {
        for id in 0..depth as u64 {
          backend.push(id, Op::Nop, &mut bump);
        }
        backend.flush().unwrap();
        backend.wait(Some(Duration::ZERO), &mut completed).unwrap();
        assert_eq!(completed.len(), depth);
        black_box(&completed);
      },
    );
  }
}

fn driver_callback(harness: &Harness) {
  for depth in QUEUE_DEPTHS {
    let lio =
      Lio::new_with_backend(ImmediateBackend::default(), depth).unwrap();
    let completed = Rc::new(Cell::new(0usize));

    harness.bench(
      &format!("bookkeeping/driver_callback/qd_{depth}"),
      depth as u64,
      || {
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
      },
    );
  }
}

fn driver_channel(harness: &Harness) {
  for depth in [1, 256] {
    let lio =
      Lio::new_with_backend(ImmediateBackend::default(), depth).unwrap();
    let (sender, receiver) = mpsc::channel();

    harness.bench(
      &format!("bookkeeping/driver_channel/qd_{depth}"),
      depth as u64,
      || {
        for _ in 0..depth {
          api::nop().with_lio(&lio).send_with(sender.clone());
        }
        assert_eq!(lio.try_run().unwrap(), depth);
        for _ in 0..depth {
          black_box(receiver.recv().unwrap().unwrap());
        }
      },
    );
  }
}

fn timers(harness: &Harness) {
  for depth in QUEUE_DEPTHS {
    let mut clock = Clock::with_capacity(depth);

    harness.bench(
      &format!("bookkeeping/timer_wheel/qd_{depth}"),
      depth as u64,
      || {
        for id in 0..depth as u64 {
          clock.schedule(id, Duration::ZERO);
        }
        clock.advance_by(1);
        assert_eq!(clock.poll_expired().count(), depth);
      },
    );
  }
}
