//! Real loopback UDP transfers through the public I/O API.
//! Uses Poller unless LIO_BENCH_DEFAULT_BACKEND is set (io_uring on Linux).
use std::time::Duration;

use criterion::{BenchmarkId, Criterion, Throughput};
use lio::Lio;
use lio::backend::impls::Poller;

#[path = "support/socket_workload.rs"]
mod socket_workload;
use socket_workload::Workload;

fn driver(depth: usize) -> Lio {
  if std::env::var_os("LIO_BENCH_DEFAULT_BACKEND").is_some() {
    Lio::new(depth * 2).unwrap()
  } else {
    Lio::new_with_backend(Poller::new(), depth * 2).unwrap()
  }
}

fn main() {
  if std::env::args().any(|arg| arg == "--count") {
    let workload = Workload::new(driver(1), 1, 64);
    for _ in 0..1000 {
      workload.batch();
    }
    println!("1000 loopback UDP transfers completed and verified");
    return;
  }
  let mut criterion = Criterion::default()
    .sample_size(50)
    .warm_up_time(Duration::from_secs(1))
    .measurement_time(Duration::from_secs(3))
    .configure_from_args();
  let mut group = criterion.benchmark_group(
    if std::env::var_os("LIO_BENCH_DEFAULT_BACKEND").is_some() {
      "default/udp_loopback"
    } else {
      "poller/udp_loopback"
    },
  );
  for bytes in [64, 4096] {
    for depth in [1, 32] {
      let workload = Workload::new(driver(depth), depth, bytes);
      group.throughput(Throughput::Elements(depth as u64));
      group.bench_function(
        BenchmarkId::new(format!("bytes_{bytes}"), depth),
        |b| b.iter(|| workload.batch()),
      );
    }
  }
  group.finish();
  criterion.final_summary();
}
